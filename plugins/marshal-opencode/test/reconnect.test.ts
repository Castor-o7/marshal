// MAR-48 against the real daemon, through the real plugin: mail sent while
// opencode's connection was down can't be pushed, so on the reconnect the
// plugin prompts it as a turn, without a new message to trigger it. It's shown
// once, and acked, so that turn's own chat.message drain and later ones don't
// show it again.
import { expect, test } from "bun:test";
import { createServer, connect, type Server, type Socket } from "node:net";
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

async function until(what: string, predicate: () => boolean | Promise<boolean>, ms = 10_000): Promise<void> {
  const deadline = Date.now() + ms;
  while (!(await predicate())) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await Bun.sleep(50);
  }
}

/** A TCP proxy in front of the daemon that can drop every connection through
 *  it, and refuse new ones, until it's opened again. */
class Cuttable {
  private server: Server | null = null;
  private readonly sockets = new Set<Socket>();
  constructor(readonly port: number, private readonly upstream: number) {}

  open(): Promise<void> {
    const server = createServer((client) => {
      const up = connect(this.upstream, "127.0.0.1");
      for (const s of [client, up]) {
        this.sockets.add(s);
        s.on("close", () => this.sockets.delete(s));
        s.on("error", () => {});
      }
      client.pipe(up);
      up.pipe(client);
      client.on("close", () => up.destroy());
      up.on("close", () => client.destroy());
    });
    this.server = server;
    return new Promise((resolve) => server.listen(this.port, "127.0.0.1", () => resolve()));
  }

  cut(): Promise<void> {
    const server = this.server;
    this.server = null;
    for (const s of this.sockets) s.destroy();
    return new Promise((resolve) => (server ? server.close(() => resolve()) : resolve()));
  }
}

test.skipIf(!process.env.MARSHAL_DAEMON_BIN)("mail that waited out a reconnect is prompted without a new message", async () => {
  const binary = process.env.MARSHAL_DAEMON_BIN;
  if (!binary) throw new Error("set MARSHAL_DAEMON_BIN to the built daemon");
  const state = mkdtempSync(join(tmpdir(), "marshal-opencode-reconnect-"));
  const ws = await freePort();
  const http = await freePort();
  const env: Record<string, string | undefined> = { ...process.env };
  delete env.MYKO_POSTGRES_URL;
  const daemonProcess = Bun.spawn([binary], {
    env: { ...env, MARSHAL_BIND: `127.0.0.1:${ws}`, MARSHAL_HOOK_BIND: `127.0.0.1:${http}`, MARSHAL_STATE_DIR: state },
    stdout: "ignore",
    stderr: "inherit",
  });
  const proxy = new Cuttable(await freePort(), ws);
  await proxy.open();
  const previousAddress = process.env.MARSHAL_DAEMON_ADDRESS;
  process.env.MARSHAL_DAEMON_ADDRESS = `ws://127.0.0.1:${proxy.port}`;
  const R = "ses_oc_reconnect_recipient";
  const S = "ses_oc_reconnect_sender";
  const sender = new MarshalDaemon({
    address: `ws://127.0.0.1:${ws}`,
    cwd: state,
    identity: { operator: "sender", host: { name: "reconnect-test", os: "linux", arch: "x64" } },
  });

  // A turn's chat.message: what the plugin appends to the user message.
  let plugin: Awaited<ReturnType<typeof MarshalPlugin>> | undefined;
  const chatMessage = async (): Promise<string> => {
    const output = { message: { sessionID: R, id: `msg_${crypto.randomUUID()}` }, parts: [] as { text: string }[] };
    await plugin!["chat.message"]!({ sessionID: R } as never, output as never);
    return output.parts.map((part) => part.text).join("");
  };

  // The opencode client the plugin calls: `prompt` runs chat.message for the
  // prompt, as opencode does, and records what that turn saw.
  const prompted: string[] = [];
  const turnDrains: string[] = [];
  const toasts: string[] = [];
  const client = {
    app: { log: async () => {} },
    tui: { showToast: async (req: { body: { message: string } }) => { toasts.push(req.body.message); } },
    session: {
      prompt: async (req: { body: { parts: { text: string }[] }; throwOnError?: boolean }) => {
        expect(req.throwOnError).toBe(true);
        prompted.push(req.body.parts.map((part) => part.text).join(""));
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

    // The connection drops, and mail arrives while it's down.
    await proxy.cut();
    await Bun.sleep(1000);
    const waited = await sender.sendMessage(S, R, "the mail that waited");
    expect(waited.deliveredLive).toBe(false);
    expect(prompted).toEqual([]);

    // It reconnects: the mail is prompted, with nothing new sent to trigger it,
    // and the prompt's own turn doesn't show it a second time.
    await proxy.open();
    await until("the waiting mail to be prompted", () => prompted.some((t) => t.includes("the mail that waited")), 30_000);
    await until("the notice, once the mail is acked", () => toasts.includes("marshal: mail that arrived while disconnected"));
    expect(turnDrains[0]).not.toContain("the mail that waited");

    // A push after the reconnect is pushed live, and neither is shown twice.
    const after = await sender.sendMessage(S, R, "the push after");
    expect(after.deliveredLive).toBe(true);
    await until("the push to be prompted", () => prompted.some((t) => t.includes("the push after")));
    await Bun.sleep(300);
    expect(prompted.filter((t) => t.includes("the mail that waited")).length).toBe(1);
    expect(prompted.filter((t) => t.includes("the push after")).length).toBe(1);
    expect(await chatMessage()).toBe("");
  } finally {
    await plugin?.dispose?.();
    sender.stop();
    await proxy.cut();
    daemonProcess.kill();
    await daemonProcess.exited;
    if (previousAddress === undefined) delete process.env.MARSHAL_DAEMON_ADDRESS;
    else process.env.MARSHAL_DAEMON_ADDRESS = previousAddress;
    rmSync(state, { recursive: true, force: true });
  }
}, 60_000);
