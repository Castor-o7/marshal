// MAR-48 against the real daemon, through the real extension: mail sent while
// Prime Agent's connection was down can't be pushed, so on the reconnect the extension
// shows it, starting a turn, without a new message to trigger it. It's shown
// once, and acked, so the next turn's pull doesn't show it again.
import { expect, test } from "bun:test";
import { createServer, connect, type Server, type Socket } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import marshalPrimeAgent from "../src/index.ts";
import { MarshalDaemon } from "../src/daemon.ts";

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

type Handler = (event: Record<string, unknown>, ctx: unknown) => unknown;

test.skipIf(!process.env.MARSHAL_DAEMON_BIN)("mail that waited out a reconnect is shown without a new message", async () => {
  const binary = process.env.MARSHAL_DAEMON_BIN;
  if (!binary) throw new Error("set MARSHAL_DAEMON_BIN to the built daemon");
  const state = mkdtempSync(join(tmpdir(), "marshal-prime-agent-reconnect-"));
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

  // The Prime Agent the extension runs in: it records what it's handed to show.
  const handlers = new Map<string, Handler[]>();
  const shown: string[] = [];
  const notes: string[] = [];
  const pi = {
    on: (name: string, handler: Handler) => handlers.set(name, [...(handlers.get(name) ?? []), handler]),
    registerTool: () => {},
    registerCommand: () => {},
    sendMessage: async (message: { content: string }, options: { triggerTurn?: boolean }) => {
      expect(options.triggerTurn).toBe(true);
      shown.push(message.content);
    },
  };
  const ctx = {
    hasUI: true,
    ui: { setStatus: () => {}, setWidget: () => {}, notify: (message: string) => notes.push(message) },
    sessionManager: { getHeader: () => ({}), getSessionId: () => R },
  };
  const emit = async (name: string, event: Record<string, unknown>) => {
    const results: unknown[] = [];
    for (const handler of handlers.get(name) ?? []) results.push(await handler(event, ctx));
    return results;
  };

  const S = "ses_prime_reconnect_sender";
  const R = "ses_prime_reconnect_recipient";
  const sender = new MarshalDaemon({
    address: `ws://127.0.0.1:${ws}`,
    cwd: state,
    identity: { operator: "sender", host: { name: "reconnect-test", os: "linux", arch: "x64" } },
  });
  try {
    await marshalPrimeAgent(pi as never);
    await emit("session_start", { type: "session_start" });
    sender.start();
    await until("the sender to connect", () => sender.isConnected());
    sender.registerSession(S);
    await until("the Prime Agent session on the roster", () => sender.rosterSync().some((s) => s.id === R));

    // The connection drops, and mail arrives while it's down.
    await proxy.cut();
    await Bun.sleep(1000);
    const waited = await sender.sendMessage(S, R, "the mail that waited");
    expect(waited.deliveredLive).toBe(false);
    expect(shown).toEqual([]);

    // It reconnects: the mail is shown, with nothing new sent to trigger it.
    await proxy.open();
    await until("the waiting mail to be shown", () => shown.some((t) => t.includes("the mail that waited")), 30_000);
    await until("the notice, once the mail is acked", () => notes.includes("marshal: mail that arrived while disconnected"));

    // A push after the reconnect is pushed live, and neither is shown twice.
    const after = await sender.sendMessage(S, R, "the push after");
    expect(after.deliveredLive).toBe(true);
    await until("the push to be shown", () => shown.some((t) => t.includes("the push after")));
    await Bun.sleep(300);
    expect(shown.filter((t) => t.includes("the mail that waited")).length).toBe(1);
    expect(shown.filter((t) => t.includes("the push after")).length).toBe(1);

    // Both were acked: the next turn's pull has nothing to add.
    const turn = await emit("before_agent_start", { type: "before_agent_start", prompt: "next", systemPrompt: "", systemPromptOptions: {} });
    expect(JSON.stringify(turn)).not.toContain("the mail that waited");
    expect(JSON.stringify(turn)).not.toContain("the push after");
  } finally {
    await emit("session_shutdown", { type: "session_shutdown" });
    sender.stop();
    await proxy.cut();
    daemonProcess.kill();
    await daemonProcess.exited;
    if (previousAddress === undefined) delete process.env.MARSHAL_DAEMON_ADDRESS;
    else process.env.MARSHAL_DAEMON_ADDRESS = previousAddress;
    rmSync(state, { recursive: true, force: true });
  }
}, 60_000);
