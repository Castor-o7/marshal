import { describe, expect, test } from "bun:test";

import { DEFAULT_HOLD_LEASE_MS, InboxPuller, holdLeaseFromEnv, pullForTurn } from "../src/inbox-pull.ts";

// An in-memory inbox: `read` returns what is still unread, `ack` marks read.
// `clock` drives the hold leases.
function fakeInbox(ids: string[], leaseMs = 1_000) {
  const unread = new Set(ids);
  const acked: string[] = [];
  const clock = { now: 0 };
  const puller = new InboxPuller<{ messageId: string }>(
    {
      read: async () => [...unread].map((messageId) => ({ messageId })),
      ack: async (_sessionId, messageIds) => {
        for (const id of messageIds) {
          unread.delete(id);
          acked.push(id);
        }
      },
      render: (messages) => messages.map((m) => m.messageId).join(","),
    },
    { leaseMs, now: () => clock.now },
  );
  return { puller, unread, acked, clock };
}

// Ported from upstream Marshal #136.
describe("inbox pull", () => {
  test("acks nothing until the pull is committed", async () => {
    const inbox = fakeInbox(["m1", "m2"]);
    const pull = await inbox.puller.pull("s");
    expect(pull?.text).toBe("m1,m2");
    expect(inbox.acked).toEqual([]);

    await pull!.commit();
    expect(inbox.acked).toEqual(["m1", "m2"]);
  });

  test("does not hand out messages another pull is still holding", async () => {
    const inbox = fakeInbox(["m1"]);
    const first = await inbox.puller.pull("s");
    expect(first?.text).toBe("m1");
    expect(await inbox.puller.pull("s")).toBeNull();

    inbox.unread.add("m2");
    expect((await inbox.puller.pull("s"))?.text).toBe("m2");
  });

  test("two pulls started together never hand out the same message", async () => {
    const inbox = fakeInbox(["m1"]);
    const pulls = await Promise.all([inbox.puller.pull("s"), inbox.puller.pull("s")]);
    expect(pulls.filter((pull) => pull !== null).map((pull) => pull!.text)).toEqual(["m1"]);
  });

  test("a released pull leaves its messages unread for the next pull", async () => {
    const inbox = fakeInbox(["m1"]);
    const first = await inbox.puller.pull("s");
    first!.release();
    expect(inbox.acked).toEqual([]);
    expect((await inbox.puller.pull("s"))?.text).toBe("m1");
  });

  test("a commit whose ack fails rejects and leaves the messages for the next pull", async () => {
    const unread = new Set(["m1"]);
    const puller = new InboxPuller<{ messageId: string }>({
      read: async () => [...unread].map((messageId) => ({ messageId })),
      ack: async () => { throw new Error("daemon gone"); },
      render: (messages) => messages.map((m) => m.messageId).join(","),
    });
    const pull = await puller.pull("s");
    await expect(pull!.commit()).rejects.toThrow("daemon gone");
    expect((await puller.pull("s"))?.text).toBe("m1");
  });

  test("a pull whose render throws leaves the messages for the next pull", async () => {
    let renders = 0;
    const puller = new InboxPuller<{ messageId: string }>({
      read: async () => [{ messageId: "m1" }],
      ack: async () => {},
      render: (messages) => {
        renders += 1;
        if (renders === 1) throw new Error("bad message");
        return messages.map((m) => m.messageId).join(",");
      },
    });
    await expect(puller.pull("s")).rejects.toThrow("bad message");
    expect((await puller.pull("s"))?.text).toBe("m1");
  });
});

describe("pull for a turn", () => {
  test("returns a pull that lands in time, still uncommitted", async () => {
    const inbox = fakeInbox(["m1"]);
    const pull = await pullForTurn(() => inbox.puller.pull("s"), 1_000);
    expect(pull?.text).toBe("m1");
    expect(inbox.acked).toEqual([]);
  });

  test("a pull that lands after the deadline leaves its messages for the next turn", async () => {
    const inbox = fakeInbox(["m1"]);
    let land!: () => void;
    const slowRead = new Promise<void>((resolve) => { land = resolve; });
    const late = pullForTurn(async () => {
      await slowRead;
      return inbox.puller.pull("s");
    }, 10);
    expect(await late).toBeNull();

    land();
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(inbox.acked).toEqual([]);
    expect((await inbox.puller.pull("s"))?.text).toBe("m1");
  });
});

// MAR-10 (from muster MUS-81): a live push holds its message while pi takes it.
describe("push hold", () => {
  test("a pull doesn't show a message a push is handing over", async () => {
    const inbox = fakeInbox(["m1"]);
    const hold = inbox.puller.holdPush("s", "m1");
    expect(hold).not.toBeNull();
    expect(await inbox.puller.pull("s")).toBeNull();
  });

  test("a committed hold acks its message once", async () => {
    const inbox = fakeInbox(["m1"]);
    const hold = inbox.puller.holdPush("s", "m1")!;
    expect(await hold.commit()).toBe(true);
    expect(inbox.acked).toEqual(["m1"]);
    expect(await inbox.puller.pull("s")).toBeNull();
  });

  test("a released hold leaves the message for the next pull", async () => {
    const inbox = fakeInbox(["m1"]);
    inbox.puller.holdPush("s", "m1")!.release();
    expect(inbox.acked).toEqual([]);
    expect((await inbox.puller.pull("s"))?.text).toBe("m1");
  });

  test("a push doesn't hold a message a pull already holds", async () => {
    const inbox = fakeInbox(["m1"]);
    const pull = await inbox.puller.pull("s");
    expect(inbox.puller.holdPush("s", "m1")).toBeNull();
    await pull!.commit();
    expect(inbox.acked).toEqual(["m1"]);
  });

  test("a hold that outlives its lease releases the message to the next pull", async () => {
    const inbox = fakeInbox(["m1"], 1_000);
    const hold = inbox.puller.holdPush("s", "m1")!;
    inbox.clock.now = 999;
    expect(await inbox.puller.pull("s")).toBeNull();

    inbox.clock.now = 1_000;
    const pull = await inbox.puller.pull("s");
    expect(pull?.text).toBe("m1");

    // The hung handover resolves late: it doesn't ack what the pull now owns.
    expect(await hold.commit()).toBe(false);
    expect(inbox.acked).toEqual([]);
    await pull!.commit();
    expect(inbox.acked).toEqual(["m1"]);
    expect(await inbox.puller.pull("s")).toBeNull();
  });

  test("a lapsed hold doesn't ack even if no pull took the message over", async () => {
    const inbox = fakeInbox(["m1"], 1_000);
    const hold = inbox.puller.holdPush("s", "m1")!;
    inbox.clock.now = 1_000;
    expect(await hold.commit()).toBe(false);
    expect((await inbox.puller.pull("s"))?.text).toBe("m1");
  });

  test("a pull's commit past its lease leaves what a later pull took over", async () => {
    const inbox = fakeInbox(["m1"], 1_000);
    const stale = await inbox.puller.pull("s");
    inbox.clock.now = 1_000;
    const fresh = await inbox.puller.pull("s");
    expect(fresh?.text).toBe("m1");
    await stale!.commit();
    expect(inbox.acked).toEqual([]);
    await fresh!.commit();
    expect(inbox.acked).toEqual(["m1"]);
  });
});

describe("hold lease", () => {
  test("defaults to the daemon's 60 s ack lease", () => {
    expect(holdLeaseFromEnv({})).toBe(DEFAULT_HOLD_LEASE_MS);
    expect(DEFAULT_HOLD_LEASE_MS).toBe(60_000);
  });

  test("follows MARSHAL_ACK_LEASE_SECS", () => {
    expect(holdLeaseFromEnv({ MARSHAL_ACK_LEASE_SECS: "2" })).toBe(2_000);
  });

  test("ignores a lease that isn't a positive number", () => {
    expect(holdLeaseFromEnv({ MARSHAL_ACK_LEASE_SECS: "soon" })).toBe(DEFAULT_HOLD_LEASE_MS);
    expect(holdLeaseFromEnv({ MARSHAL_ACK_LEASE_SECS: "0" })).toBe(DEFAULT_HOLD_LEASE_MS);
  });
});
