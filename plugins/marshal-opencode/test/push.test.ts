import { describe, expect, test } from "bun:test";

import { deliverPush, deliverWaiting, pushText, type PushTarget } from "../src/push.js";

describe("live push text", () => {
  test("a whole body is shown as is", () => {
    expect(pushText("alice", { body: "hi", body_truncated: false, message_id: "m1" })).toBe(
      "new message from alice [response_required=false]: hi",
    );
  });

  test("a body that requires a response says so", () => {
    expect(pushText("alice", { body: "hi", response_required: true })).toBe(
      "new message from alice [response_required=true]: hi",
    );
  });

  test("a truncated body says so and names the message", () => {
    expect(pushText("alice", { body: "the first part", body_truncated: true, message_id: "m1" })).toBe(
      "new message from alice [response_required=false]: the first part\n" +
        "[preview truncated; read the full message with marshal_messages (message m1)]",
    );
  });

  test("a truncated body without an id still says so", () => {
    expect(pushText("alice", { body: "the first part", body_truncated: true })).toEndWith(
      "[preview truncated; read the full message with marshal_messages]",
    );
  });
});

// MAR-10: a push is acked only once opencode has taken it.
describe("deliverPush", () => {
  function fakeTarget(opts: { promptFails?: boolean; inbox?: string | null; pulled?: string[] } = {}) {
    const calls: string[] = [];
    const held = new Set<string>(opts.pulled);
    const target: PushTarget = {
      prompt: async (text) => {
        calls.push(`prompt:${text}`);
        if (opts.promptFails) throw new Error("opencode lost it");
      },
      hold: (id) => {
        if (held.has(id)) return null;
        held.add(id);
        calls.push(`hold:${id}`);
        return () => {
          held.delete(id);
          calls.push(`release:${id}`);
        };
      },
      ack: async (id) => {
        calls.push(`ack:${id}`);
      },
      drain: async () => opts.inbox ?? null,
    };
    return { target, calls, held };
  }

  test("acks the pushed message once the prompt resolves, holding it meanwhile", async () => {
    const t = fakeTarget();
    expect(await deliverPush({ body: "hi", message_id: "m1" }, "alice", t.target)).toBe(true);
    expect(t.calls).toEqual([
      "hold:m1",
      "prompt:new message from alice [response_required=false]: hi",
      "ack:m1",
      "release:m1",
    ]);
    expect(t.held.size).toBe(0);
  });

  test("acks nothing when the prompt fails, and releases the hold", async () => {
    const t = fakeTarget({ promptFails: true });
    await expect(deliverPush({ body: "hi", message_id: "m1" }, "alice", t.target)).rejects.toThrow("opencode lost it");
    expect(t.calls.some((c) => c.startsWith("ack:"))).toBe(false);
    expect(t.calls.at(-1)).toBe("release:m1");
    expect(t.held.size).toBe(0);
  });

  test("a push that names no message is prompted, and holds and acks nothing", async () => {
    const t = fakeTarget();
    await deliverPush({ body: "hi" }, "alice", t.target);
    expect(t.calls).toEqual(["prompt:new message from alice [response_required=false]: hi"]);
  });

  test("a push with no body prompts the inbox, as before", async () => {
    const t = fakeTarget({ inbox: "new message from bob: hello" });
    expect(await deliverPush({ message_id: "m1" }, "alice", t.target)).toBe(true);
    expect(t.calls).toEqual(["prompt:new message from bob: hello"]);
  });

  // MAR-48: a reconnect's pull already holds it, and shows it.
  test("a push a pull already holds is left to the pull", async () => {
    const t = fakeTarget({ pulled: ["m1"] });
    expect(await deliverPush({ body: "hi", message_id: "m1" }, "alice", t.target)).toBe(false);
    expect(t.calls).toEqual([]);
  });

  test("a push with no body and an empty inbox prompts nothing", async () => {
    const t = fakeTarget();
    expect(await deliverPush({}, "alice", t.target)).toBe(false);
    expect(t.calls).toEqual([]);
  });
});

// MAR-48: on a reconnect, the mail that waited out the drop is prompted and acked.
describe("deliverWaiting", () => {
  function fakePull(text: string | null) {
    const calls: string[] = [];
    const pull = async () =>
      text === null
        ? null
        : {
            text,
            commit: async () => { calls.push("commit"); },
            release: () => { calls.push("release"); },
          };
    return { pull, calls };
  }

  test("prompts the unread mail and acks it once opencode has taken it", async () => {
    const p = fakePull("2 new messages");
    const prompted: string[] = [];
    expect(await deliverWaiting({ pull: p.pull, prompt: async (t) => { prompted.push(t); } })).toBe(true);
    expect(prompted).toEqual(["2 new messages"]);
    expect(p.calls).toEqual(["commit"]);
  });

  test("prompts nothing when nothing is unread", async () => {
    const p = fakePull(null);
    const prompted: string[] = [];
    expect(await deliverWaiting({ pull: p.pull, prompt: async (t) => { prompted.push(t); } })).toBe(false);
    expect(prompted).toEqual([]);
  });

  test("acks nothing when the prompt fails, and gives the mail back", async () => {
    const p = fakePull("1 new message");
    await expect(
      deliverWaiting({ pull: p.pull, prompt: async () => { throw new Error("opencode lost it"); } }),
    ).rejects.toThrow("opencode lost it");
    expect(p.calls).toEqual(["release"]);
  });
});
