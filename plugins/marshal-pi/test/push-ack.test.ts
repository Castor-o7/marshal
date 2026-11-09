// MAR-10 against the real daemon: a pi session registers with acksPushes, so
// a live push it fails to hand to pi stays unread, and the next turn's pull
// shows it. One it does hand over is acked and isn't shown again.
import { expect, test } from "bun:test";
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { MarshalDaemon } from "../src/daemon.ts";
import type { NotifyChannelMeta } from "../src/entities.ts";
import { deliverLivePush } from "../src/live-push.ts";

async function freePort(): Promise<number> {
  const server = createServer();
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (!address || typeof address === "string") throw new Error("expected TCP address");
      server.close(() => resolve(address.port));
    });
  });
}

async function until(what: string, predicate: () => boolean | Promise<boolean>): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (!(await predicate())) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await Bun.sleep(50);
  }
}

const identity = (operator: string) => ({ operator, host: { name: "push-ack-test", os: "linux", arch: "x64" } });

test.skipIf(!process.env.MARSHAL_DAEMON_BIN)("a push pi fails to take stays unread for the next pull", async () => {
  const binary = process.env.MARSHAL_DAEMON_BIN;
  if (!binary) throw new Error("set MARSHAL_DAEMON_BIN to the built daemon");
  const state = mkdtempSync(join(tmpdir(), "marshal-pi-push-ack-"));
  const ws = await freePort();
  const http = await freePort();
  const env: Record<string, string | undefined> = { ...process.env };
  delete env.MYKO_POSTGRES_URL;
  const daemonProcess = Bun.spawn([binary], {
    env: { ...env, MARSHAL_BIND: `127.0.0.1:${ws}`, MARSHAL_HOOK_BIND: `127.0.0.1:${http}`, MARSHAL_STATE_DIR: state },
    stdout: "ignore",
    stderr: "inherit",
  });
  const address = `ws://127.0.0.1:${ws}`;
  const recipient = new MarshalDaemon({ address, cwd: state, identity: identity("recipient") });
  const sender = new MarshalDaemon({ address, cwd: state, identity: identity("sender") });
  const R = "ses_push_ack_recipient";
  const S = "ses_push_ack_sender";

  // What pi does with each push: fail to take it while `piFails`, else take it.
  let piFails = true;
  const injected: string[] = [];
  const delivered: Array<Promise<void>> = [];
  recipient.onNotify((meta: NotifyChannelMeta) => {
    delivered.push(
      deliverLivePush(meta, meta.from_nickname ?? "sender", {
        inject: async (content) => {
          if (piFails) throw new Error("pi lost it");
          injected.push(content);
        },
        hold: (id) => recipient.holdPush(R, id),
        pull: () => recipient.pullInbox(R),
      }).catch(() => {}),
    );
  });

  try {
    recipient.start();
    sender.start();
    await until("both clients to connect", () => recipient.isConnected() && sender.isConnected());
    recipient.registerSession(R);
    sender.registerSession(S);
    await until("both sessions on the roster", () => {
      const ids = sender.rosterSync().map((s) => s.id);
      return ids.includes(R) && ids.includes(S);
    });
    const row = sender.rosterSync().find((s) => s.id === R);
    expect(row?.acksPushes).toBe(true);

    // 1. pi fails to take the push: it stays unread, and the next pull shows it.
    const lost = await sender.sendMessage(S, R, "the push pi dropped");
    expect(lost.deliveredLive).toBe(true);
    await until("the push to reach the recipient", () => delivered.length === 1);
    await Promise.all(delivered);
    expect(injected).toEqual([]);
    const pull = await recipient.pullInbox(R);
    expect(pull?.text).toContain("the push pi dropped");
    await pull!.commit();
    expect(await recipient.pullInbox(R)).toBeNull();

    // 2. pi takes the push: it's acked, and no pull shows it again.
    piFails = false;
    await sender.sendMessage(S, R, "the push pi took");
    await until("the second push to reach the recipient", () => delivered.length === 2);
    await Promise.all(delivered);
    expect(injected.length).toBe(1);
    expect(injected[0]).toContain("the push pi took");
    await until("the ack to land", async () => (await recipient.pullInbox(R).then((p) => { p?.release(); return p; })) === null);
  } finally {
    recipient.stop();
    sender.stop();
    daemonProcess.kill();
    await daemonProcess.exited;
    rmSync(state, { recursive: true, force: true });
  }
}, 30_000);
