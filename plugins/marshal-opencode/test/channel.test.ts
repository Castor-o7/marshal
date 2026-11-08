// MAR-13: a connected opencode session is woken by the daemon's live push, so
// the plugin declares its channel and the roster says `channel` for it, not
// `none`. Runs the real daemon and reads the roster through `marshal-shim
// roster`, which is where the `wake` word is worked out.
import { expect, test } from "bun:test";
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
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

async function until(what: string, predicate: () => boolean | Promise<boolean>): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (!(await predicate())) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await Bun.sleep(50);
  }
}

test.skipIf(!process.env.MARSHAL_DAEMON_BIN)("a connected opencode session reads `channel` on the roster", async () => {
  const binary = process.env.MARSHAL_DAEMON_BIN;
  if (!binary) throw new Error("set MARSHAL_DAEMON_BIN to the built daemon");
  const shim = join(dirname(binary), "marshal-shim");
  // A home of its own, so no daemon-address file on this machine points the
  // shim somewhere else.
  const home = mkdtempSync(join(tmpdir(), "marshal-opencode-channel-"));
  const ws = await freePort();
  const http = await freePort();
  const env: Record<string, string | undefined> = {
    ...process.env,
    HOME: home,
    XDG_CONFIG_HOME: join(home, ".config"),
    APPDATA: join(home, "AppData"),
  };
  delete env.MYKO_POSTGRES_URL;
  delete env.MYKO_ADDRESS;
  const daemonProcess = Bun.spawn([binary], {
    env: { ...env, MARSHAL_BIND: `127.0.0.1:${ws}`, MARSHAL_HOOK_BIND: `127.0.0.1:${http}`, MARSHAL_STATE_DIR: home },
    stdout: "ignore",
    stderr: "inherit",
  });
  const address = `ws://127.0.0.1:${ws}`;
  const roster = async (): Promise<Array<Record<string, unknown>>> => {
    const read = Bun.spawn([shim, "roster"], {
      env: { ...env, MARSHAL_DAEMON_ADDRESS: address },
      stdout: "pipe",
      stderr: "pipe",
    });
    const out = await new Response(read.stdout).text();
    if ((await read.exited) !== 0) return [];
    return (JSON.parse(out).sessions ?? []) as Array<Record<string, unknown>>;
  };
  const client = new MarshalDaemon({
    address,
    cwd: home,
    identity: { operator: "channel-test", host: { name: "channel-test", os: "linux", arch: "x64" } },
  });
  try {
    client.start();
    await until("the plugin to connect", () => client.isConnected());
    client.registerSession("ses_channel");
    await until("the session to reach the roster", () => client.rosterSync().some((s) => s.id === "ses_channel"));

    // The declaration reaches the daemon's record...
    const row = client.rosterSync().find((s) => s.id === "ses_channel");
    expect(row?.channelsEnabled).toBe(true);

    // ...and the roster's wake word follows from it.
    let wake: unknown;
    await until("a roster row for the session", async () => {
      wake = (await roster()).find((s) => s.session_id === "ses_channel")?.wake;
      return wake !== undefined;
    });
    expect(wake).toBe("channel");
  } finally {
    client.stop();
    daemonProcess.kill();
    await daemonProcess.exited;
    rmSync(home, { recursive: true, force: true });
  }
}, 30_000);
