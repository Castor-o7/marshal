#!/usr/bin/env bash
# MAR-75: drive direct mail to the test sessions for a window, with an hourly canary.
#   drive.sh [hours] [load_every_min] [canary_every_min]     defaults 24, 20, 60
# Load: every load_every_min, one `tell` to the next session in turn.
# Canary: every canary_every_min, one `tell` to every session, each timed:
#   taken = the db's first MessageRead for it (read-only), within 60 s;
#   woke  = the session's roster waiting_until changed, so it finished a turn and
#           holds a new waiter, within 60 s.
# With the defaults: 72 load + 72 canary = 144 direct messages in 24 h, one short
# model turn each. Run it in its own tmux window; it logs to $STATE.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
need_sessions
HOURS=${1:-24} LOAD=${2:-20} CANARY=${3:-60}
SENDS=$STATE/sends.tsv CAN=$STATE/canary.tsv
[ -s "$SENDS" ] || printf 'sent_ms\tkind\tnickname\ttoken\ttell\n' > "$SENDS"
[ -s "$CAN" ] || printf 'sent_ms\tnickname\ttoken\ttaken_ms\ttaken_s\twoke\n' > "$CAN"
mapfile -t NICKS < <(cut -f3 "$SESSIONS")
mapfile -t SIDS < <(cut -f4 "$SESSIONS")

send() {  # kind nickname -> token
  local tok="soak $1 $2 $(now_ms)" out
  out=$("$SHIM" tell "$2" "$tok" 2>&1 | head -1 | tr '\t' ' ')
  printf '%s\t%s\t%s\t%s\t%s\n' "$(now_ms)" "$1" "$2" "$tok" "$out" >> "$SENDS"
  echo "$tok"
}

canary_round() {
  local i tok sent before after state taken woke
  for i in "${!NICKS[@]}"; do
    before=$(roster_field "${SIDS[$i]}" waiting_until)
    sent=$(now_ms)
    tok=$(send canary "${NICKS[$i]}")
    taken=- woke=no
    for _ in $(seq 1 20); do
      sleep 3
      state=$(token_state "$tok"); [ "${state#* }" != - ] && taken=${state#* }
      after=$(roster_field "${SIDS[$i]}" waiting_until)
      [ "$after" != "$before" ] && [ "$after" != None ] && woke=yes
      [ "$taken" != - ] && [ "$woke" = yes ] && break
    done
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$sent" "${NICKS[$i]}" "$tok" "$taken" \
      "$([ "$taken" = - ] && echo - || awk -v a="$taken" -v b="$sent" 'BEGIN{printf "%.1f", (a-b)/1000}')" "$woke" >> "$CAN"
    log "canary ${NICKS[$i]}: taken=$taken woke=$woke"
  done
}

end=$(awk -v h="$HOURS" -v t="$(date +%s)" 'BEGIN{printf "%d", t + h * 3600}')   # hours may be fractional, for a dry run
next_load=$(date +%s) next_canary=$(date +%s) n=0
log "driving ${#NICKS[@]} sessions for ${HOURS} h: load every ${LOAD} min, canary every ${CANARY} min"
while [ "$(date +%s)" -lt "$end" ]; do
  t=$(date +%s)
  if [ "$t" -ge "$next_canary" ]; then canary_round; next_canary=$(( t + CANARY * 60 )); fi
  if [ "$t" -ge "$next_load" ]; then
    send load "${NICKS[$(( n % ${#NICKS[@]} ))]}" >/dev/null; n=$(( n + 1 ))
    next_load=$(( t + LOAD * 60 ))
  fi
  sleep 20
done
log "window over: $(($(wc -l < "$SENDS") - 1)) messages sent"
