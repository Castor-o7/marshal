// How a live push reaches the session. Kept free of pi and daemon imports so
// it can be tested with a fake inbox.

import type { NotifyChannelMeta } from "./entities.ts";
import type { InboxPull, PushHold } from "./inbox-pull.ts";
import { livePushText } from "./inbound.ts";

export interface LivePushTarget {
  /** Hand `content` to pi as a steer message. Resolves once pi has taken it. */
  inject(content: string): Promise<void> | void;
  /** Hold the pushed message while pi takes it, so a pull doesn't also show
   *  it. Null when a pull already holds it, and then the push isn't shown. */
  hold(messageId: string): PushHold | null;
  /** Read this session's unread direct messages, unacked. */
  pull(): Promise<InboxPull | null>;
}

export async function deliverLivePush(meta: NotifyChannelMeta, who: string, target: LivePushTarget): Promise<void> {
  const text = livePushText(meta, who);
  if (text) {
    // Don't drain here: that acks any other unread message without showing
    // it. The session registers with acksPushes, so the daemon leaves this
    // message unread until we ack it, once pi has taken it (MAR-10). If pi
    // throws, it stays unread for the next turn's pull. A room @mention is
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
