// Inbox pulls and live pushes that ack a message only once it has been shown.
//
// Acking in the same step as the read loses messages whenever the caller ends
// up not showing what it read: a per-turn pull that loses its timeout race, or
// an injection that fails. A pull here hands the messages back unacked; the
// caller commits it once the text is in the conversation, or releases it so
// the next pull hands the same messages out again.
//
// A live push holds its message the same way while pi takes it, so a pull in
// the meantime doesn't show it a second time (MAR-10, from muster's MUS-81).
// Every hold lapses after a lease: a handover that never resolves would
// otherwise keep its message unread and held, so no pull would ever show it.
// Once a hold has lapsed, the next pull owns the message, and the late
// handover no longer acks it.

/** Unread messages read but not yet acked. */
export interface InboxPull {
  /** The rendered messages, ready to inject. */
  readonly text: string;
  /** Ack the messages it still holds. Call once `text` has been shown. */
  commit(): Promise<void>;
  /** Give the messages back unacked, so the next pull hands them out again. */
  release(): void;
}

/** One message held by a live push while pi takes it. */
export interface PushHold {
  /** Ack the message if this hold still owns it, then let it go. Returns
   *  whether it acked: false once the lease has lapsed or a pull took it over. */
  commit(): Promise<boolean>;
  /** Let the message go unacked, so the next pull shows it. */
  release(): void;
}

/** Where a pull reads, acks and renders messages. */
export interface InboxSource<M extends { messageId: string }> {
  read(sessionId: string): Promise<M[] | null>;
  ack(sessionId: string, messageIds: string[]): Promise<void>;
  render(messages: M[]): string;
}

/** How long a hold lasts by default: the daemon's ack lease for hooks. */
export const DEFAULT_HOLD_LEASE_MS = 60_000;

/** The hold lease from `MARSHAL_ACK_LEASE_SECS`, as the daemon reads it. */
export function holdLeaseFromEnv(env: Record<string, string | undefined> = process.env): number {
  const secs = Number(env.MARSHAL_ACK_LEASE_SECS);
  return Number.isFinite(secs) && secs > 0 ? secs * 1000 : DEFAULT_HOLD_LEASE_MS;
}

/** A held id, and until when. Compared by identity, so a later hold on the
 *  same id is a different one. */
interface Hold {
  readonly until: number;
}

export class InboxPuller<M extends { messageId: string }> {
  /** Per session, the ids a pull or a push holds: still unread, so a read returns them. */
  private readonly held = new Map<string, Map<string, Hold>>();
  /** Per session, the pull in flight. Pulls run one at a time, so each sees what the last one holds. */
  private readonly pulling = new Map<string, Promise<unknown>>();

  private readonly source: InboxSource<M>;
  private readonly leaseMs: number;
  private readonly now: () => number;

  constructor(source: InboxSource<M>, opts: { leaseMs?: number; now?: () => number } = {}) {
    this.source = source;
    this.leaseMs = opts.leaseMs ?? DEFAULT_HOLD_LEASE_MS;
    this.now = opts.now ?? Date.now;
  }

  pull(sessionId: string): Promise<InboxPull | null> {
    const previous = this.pulling.get(sessionId) ?? Promise.resolve();
    const next = previous.catch(() => {}).then(() => this.pullNow(sessionId));
    this.pulling.set(sessionId, next);
    void next.catch(() => {}).finally(() => {
      if (this.pulling.get(sessionId) === next) this.pulling.delete(sessionId);
    });
    return next;
  }

  /**
   * Hold one pushed message while pi takes it. Returns null when a pull
   * already holds it: that pull owns it, and acks it when it's shown.
   */
  holdPush(sessionId: string, messageId: string): PushHold | null {
    const held = this.heldFor(sessionId);
    if (this.isHeld(held, messageId)) return null;
    const hold = this.take(held, [messageId]);
    return {
      commit: async () => {
        if (!this.owns(held, messageId, hold)) return false;
        try {
          await this.source.ack(sessionId, [messageId]);
          return true;
        } finally {
          this.settle(held, [messageId], hold);
        }
      },
      release: () => this.settle(held, [messageId], hold),
    };
  }

  private async pullNow(sessionId: string): Promise<InboxPull | null> {
    const held = this.heldFor(sessionId);
    const messages = (await this.source.read(sessionId))?.filter((m) => !this.isHeld(held, m.messageId));
    if (!messages?.length) return null;
    // Render before holding anything, so a render that throws holds nothing.
    const text = this.source.render(messages);
    const ids = messages.map((m) => m.messageId);
    const hold = this.take(held, ids);
    return {
      text,
      commit: async () => {
        // Ack only what this pull still holds: past its lease, a later pull
        // may have taken a message over.
        const mine = ids.filter((id) => this.owns(held, id, hold));
        if (!mine.length) return;
        try {
          await this.source.ack(sessionId, mine);
        } finally {
          this.settle(held, mine, hold);
        }
      },
      release: () => this.settle(held, ids, hold),
    };
  }

  private heldFor(sessionId: string): Map<string, Hold> {
    let held = this.held.get(sessionId);
    if (!held) {
      held = new Map();
      this.held.set(sessionId, held);
    }
    return held;
  }

  private isHeld(held: Map<string, Hold>, id: string): boolean {
    const hold = held.get(id);
    return hold !== undefined && hold.until > this.now();
  }

  private take(held: Map<string, Hold>, ids: string[]): Hold {
    const hold: Hold = { until: this.now() + this.leaseMs };
    for (const id of ids) held.set(id, hold);
    return hold;
  }

  /** Whether `hold` still owns `id`: nobody took it over and its lease hasn't lapsed. */
  private owns(held: Map<string, Hold>, id: string, hold: Hold): boolean {
    return held.get(id) === hold && hold.until > this.now();
  }

  private settle(held: Map<string, Hold>, ids: string[], hold: Hold): void {
    for (const id of ids) if (held.get(id) === hold) held.delete(id);
  }
}

/**
 * Pull for a turn, giving up after `timeoutMs` so the turn is never held up.
 * A pull that lands after that is released, not acked: its messages were not
 * shown, so they stay unread for the next turn.
 */
export async function pullForTurn(pull: () => Promise<InboxPull | null>, timeoutMs: number): Promise<InboxPull | null> {
  const pending = pull().catch(() => null);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<"late">((resolve) => { timer = setTimeout(() => resolve("late"), timeoutMs); });
  const winner = await Promise.race([pending, deadline]);
  clearTimeout(timer);
  if (winner !== "late") return winner;
  void pending.then((landed) => landed?.release());
  return null;
}
