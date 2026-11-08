// How a live push reaches the session. Kept free of pi and daemon imports so
// it can be tested with a fake inbox.

import type { NotifyChannelMeta } from "./entities.ts";
import type { InboxPull, PushHold } from "./inbox-pull.ts";

export interface LivePushTarget {
  /** Hand `content` to Prime Agent as a steer message. Resolves once it has taken it. */
  inject(content: string): Promise<void> | void;
  /** Hold the pushed message while Prime Agent takes it, so a pull doesn't also show
   *  it. Null when a pull already holds it, and then the push isn't shown. */
  hold(messageId: string): PushHold | null;
  /** Read this session's unread direct messages, unacked. */
  pull(): Promise<InboxPull | null>;
}

/** The steer message for a live push, or `undefined` when the push carries no
 *  body (older daemons), in which case the caller pulls the inbox instead.
 *  A body the daemon cut short says so, and names the message. */
export function livePushText(meta: NotifyChannelMeta, who: string): string | undefined {
  if (!meta.body) return undefined;
  const text = `new message from ${who} [response_required=${meta.response_required ?? false}]: ${meta.body}`;
  if (!meta.body_truncated) return text;
  const id = meta.message_id ? ` (message ${meta.message_id})` : "";
  return `${text}\n[preview truncated; read the full message with marshal_messages${id}]`;
}

export async function deliverLivePush(meta: NotifyChannelMeta, who: string, target: LivePushTarget): Promise<void> {
  const text = livePushText(meta, who);
  if (text) {
    // Don't drain here: that acks any other unread message without showing
    // it. The session registers with acksPushes, so the daemon leaves this
    // message unread until we ack it, once Prime Agent has taken it (MAR-10). If
    // it throws, it stays unread for the next turn's pull. A room @mention is
    // left unread by the daemon either way, and is acked the same way.
    const hold = meta.message_id ? target.hold(meta.message_id) : null;
    // No hold means a pull already holds it, for the turn that's starting:
    // that turn shows it, so don't show it twice. If that pull is released
    // unshown, the next pull shows it.
    if (meta.message_id && !hold) return;
    try {
      await target.inject(text);
    } catch (e) {
      hold?.release();
      throw e;
    }
    await hold?.commit().catch(() => hold.release());
    return;
  }
  // Older daemons send no body: show the inbox instead, and ack it once shown.
  const pull = await target.pull().catch(() => null);
  if (!pull) return;
  try {
    await target.inject(pull.text);
  } catch (e) {
    pull.release();
    throw e;
  }
  await pull.commit().catch(() => pull.release());
}
