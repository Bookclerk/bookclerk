/**
 * Orders overlapping reads of one resource so an older answer never
 * replaces a newer one.
 *
 * Each read takes a ticket when it starts. When it answers, its result is
 * applied only if no read that started later has already been applied, so
 * whichever writers race (a page refresh, a poll, a live update), the
 * screen ends on the newest data any of them read.
 */
export class LatestRead {
  /** Tickets handed out so far; the next read gets `issued + 1`. */
  private issued = 0;

  /** Ticket of the newest read applied, 0 before any. */
  private applied = 0;

  /**
   * Starts a read.
   *
   * @returns The read's ticket, to pass to {@link accept} with its answer.
   */
  begin(): number {
    this.issued += 1;
    return this.issued;
  }

  /**
   * Whether the read holding `ticket` may apply its answer, recording it as
   * the newest applied when it may.
   *
   * @param ticket - The ticket {@link begin} returned for the read.
   * @returns True when no later-started read was applied before it.
   */
  accept(ticket: number): boolean {
    if (ticket <= this.applied) return false;
    this.applied = ticket;
    return true;
  }
}
