// Kept out of index.ts: opencode treats every function a plugin module
// exports as a plugin.

import type { NotifyChannelMeta } from "./entities.js";

/** The turn text for a live push. `meta.body` is a bounded preview; when the
 *  daemon cut it, say so and name the message, so the agent reads the rest
 *  with marshal_messages instead of acting on half a message. */
export function pushText(who: string, meta: NotifyChannelMeta): string {
  const text = `new message from ${who} [response_required=${meta.response_required ?? false}]: ${meta.body ?? ""}`;
  if (!meta.body_truncated) return text;
  const id = meta.message_id ? ` (message ${meta.message_id})` : "";
  return `${text}\n[preview truncated; read the full message with marshal_messages${id}]`;
}

/** What delivering a push needs from the plugin. */
export interface PushTarget {
  /** Run `text` as a turn. Resolves once opencode has taken it. */
  prompt(text: string): Promise<unknown>;
  /** Hold `messageId` out of inbox drains while it's being prompted, so the
   *  turn's own chat.message drain doesn't show it a second time. Returns the
   *  release, or null when a pull already holds it, and then the push isn't
   *  shown: the pull shows it. */
  hold(messageId: string): (() => void) | null;
  /** Mark one message read. */
  ack(messageId: string): Promise<unknown>;
  /** Read, ack and render the unread inbox (older daemons' bodiless push). */
  drain(): Promise<string | null>;
}

/**
 * Deliver a live push as a turn (MAR-10). The session registers with
 * acksPushes, so the daemon leaves the pushed message unread until this acks
 * it, which it does only once `prompt` resolves. If opencode doesn't take it,
 * the message stays unread and the next turn's drain shows it.
 * Returns false when there was nothing to show.
 */
export async function deliverPush(meta: NotifyChannelMeta, who: string, target: PushTarget): Promise<boolean> {
  if (!meta.body) {
    // Older daemons send no body: show the inbox instead, as before.
    const inbox = await target.drain();
    if (!inbox) return false;
    await target.prompt(inbox);
    return true;
  }
  const id = meta.message_id;
  const release = id ? target.hold(id) : undefined;
  if (id && !release) return false;
  try {
    await target.prompt(pushText(who, meta));
    if (id) await target.ack(id);
  } finally {
    release?.();
  }
  return true;
}

/** What showing mail that waited out a reconnect needs from the plugin. */
export interface WaitingTarget {
  /** Run `text` as a turn. Resolves once opencode has taken it. */
  prompt(text: string): Promise<unknown>;
  /** Read the unread inbox, unacked (`MarshalDaemon.pullInbox`). */
  pull(): Promise<{ text: string; commit(): Promise<void>; release(): void } | null>;
}

/**
 * On a reconnect, show the mail that waited out the drop as a turn, and ack it
 * once opencode has taken it (MAR-48). If the prompt fails, the mail stays
 * unread for the next turn's drain. Returns whether there was anything to show.
 */
export async function deliverWaiting(target: WaitingTarget): Promise<boolean> {
  const pull = await target.pull().catch(() => null);
  if (!pull) return false;
  try {
    await target.prompt(pull.text);
  } catch (e) {
    pull.release();
    throw e;
  }
  await pull.commit().catch(() => pull.release());
  return true;
}
