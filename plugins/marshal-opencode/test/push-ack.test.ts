// MAR-10 against the real daemon, through the real plugin: the session
// registers with acksPushes, so a push whose prompt fails stays unread and the
// next turn's chat.message drain shows it. A push whose prompt succeeds is
// acked, and the chat.message that opencode runs for that prompt doesn't show
// it a second time.
import { expect, test } from "bun:test";
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { MarshalDaemon } from "../src/daemon.ts";
import { MarshalPlugin } from "../src/index.ts";

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

test.skipIf(!process.env.MARSHAL_DAEMON_BIN)("a push opencode fails to take stays unread for the next turn", async () => {
  const binary = process.env.MARSHAL_DAEMON_BIN;
  if (!binary) throw new Error("set MARSHAL_DAEMON_BIN to the built daemon");
  const state = mkdtempSync(join(tmpdir(), "marshal-opencode-push-ack-"));
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
  const previousAddress = process.env.MARSHAL_DAEMON_ADDRESS;
  process.env.MARSHAL_DAEMON_ADDRESS = address;
  const R = "ses_oc_push_ack_recipient";
  const S = "ses_oc_push_ack_sender";
  const sender = new MarshalDaemon({
    address,
    cwd: state,
    identity: { operator: "sender", host: { name: "push-ack-test", os: "linux", arch: "x64" } },
  });

  // A turn's chat.message: what the plugin appends to the user message.
  let plugin: Awaited<ReturnType<typeof MarshalPlugin>> | undefined;
  const chatMessage = async (): Promise<string> => {
    const output = { message: { sessionID: R, id: `msg_${crypto.randomUUID()}` }, parts: [] as { text: string }[] };
    await plugin!["chat.message"]!({ sessionID: R } as never, output as never);
    return output.parts.map((part) => part.text).join("");
  };

  // The opencode client the plugin calls. `prompt` fails while `promptFails`;
  // otherwise it runs chat.message for the prompt, as opencode does, and
  // records what that turn saw.
  let promptFails = true;
  const prompted: string[] = [];
  const turnDrains: string[] = [];
  const client = {
    app: { log: async () => {} },
    tui: { showToast: async () => {} },
    session: {
      prompt: async (req: { body: { parts: { text: string }[] }; throwOnError?: boolean }) => {
        expect(req.throwOnError).toBe(true);
        const text = req.body.parts.map((part) => part.text).join("");
        prompted.push(text);
        if (promptFails) throw new Error("opencode lost it");
        turnDrains.push(await chatMessage());
      },
    },
  };

  try {
    plugin = await MarshalPlugin({ client, $: Bun.$, directory: state, worktree: "" } as never);
    await plugin.event!({ event: { type: "session.created", properties: { info: { id: R } } } } as never);
    sender.start();
    await until("the sender to connect", () => sender.isConnected());
    sender.registerSession(S);
    await until("the recipient on the roster", () => sender.rosterSync().some((s) => s.id === R));
    expect(sender.rosterSync().find((s) => s.id === R)?.acksPushes).toBe(true);

    // 1. The prompt fails: the message stays unread, and the next turn shows it.
    let res = await sender.sendMessage(S, R, "the push opencode dropped");
    expect(res.deliveredLive).toBe(true);
    await until("the first prompt", () => prompted.length === 1);
    await Bun.sleep(300); // let a wrong ack, if any, land
    expect(await chatMessage()).toContain("the push opencode dropped");
    expect(await chatMessage()).toBe("");

    // 2. The prompt succeeds: its own turn's drain doesn't show the pushed
    //    message again, and once it's acked no later turn does either.
    promptFails = false;
    res = await sender.sendMessage(S, R, "the push opencode took");
    expect(res.deliveredLive).toBe(true);
    await until("the second turn", () => turnDrains.length === 1);
    expect(prompted[1]).toContain("the push opencode took");
    expect(turnDrains[0]).not.toContain("the push opencode took");
    await Bun.sleep(300);
    expect(await chatMessage()).toBe("");
  } finally {
    await plugin?.dispose?.();
    sender.stop();
    daemonProcess.kill();
    await daemonProcess.exited;
    if (previousAddress === undefined) delete process.env.MARSHAL_DAEMON_ADDRESS;
    else process.env.MARSHAL_DAEMON_ADDRESS = previousAddress;
    rmSync(state, { recursive: true, force: true });
  }
}, 30_000);
