#!/usr/bin/env bash
# MAR-75: the planned restart with mail provably unread, by MAR-70 run 2's method
# (docs/cutover-test-session.md, check 4). On one test session (default s1):
#   1. stop its waiters, so nothing takes its mail;
#   2. send a token, and check the db (read-only) has no MessageRead for it;
#   3. wait for the lamplighter to restart marshal-daemon (this script never does);
#   4. check the db again: still unread;
#   5. prompt the session, and check the token is taken once, after the restart.
# Exits non-zero, and says why, if any step fails.
#   restart-check.sh [window]
set -euo pipefail
. "$(dirname "$0")/lib.sh"
need_sessions
W=${1:-s1}
row=$(awk -v w="$W" '$1 == w' "$SESSIONS"); [ -n "$row" ] || { log "no session $W"; exit 1; }
NICK=$(cut -f3 <<<"$row") SID=$(cut -f4 <<<"$row")
OUT=$STATE/restart.tsv
fail() { log "FAIL: $*"; printf '%s\tFAIL\t%s\n' "$(now_ms)" "$*" >> "$OUT"; exit 1; }

C=$(claude_pid "$W"); [ -n "$C" ] || fail "no claude process in $SOAK_TMUX:$W"
waits=$(ps -o pid=,args= --ppid "$C" | awk '/claude-hook wait/ {print $1}')
[ -n "$waits" ] || fail "$NICK has no waiter to stop (is it idle after a reply?)"
log "stopping $NICK's waiters: $(echo $waits)"
kill -TERM $waits
sleep 3
[ "$(roster_field "$SID" wake)" = none ] || fail "$NICK still shows a waiter"

tok="soak restart $NICK $(now_ms)"
"$SHIM" tell "$NICK" "$tok" | head -1
sleep 3
read -r mid r0 <<<"$(token_state "$tok")"
[ "$mid" != - ] || fail "token not in the db"
[ "$r0" = - ] || fail "token already taken before the restart ($r0)"
log "token $mid unread. Ask the lamplighter now: systemctl --user restart marshal-daemon"

pid0=$(systemctl --user show marshal-daemon -p MainPID --value)
until [ "$(systemctl --user show marshal-daemon -p MainPID --value)" != "$pid0" ]; do sleep 5; done
restart_ms=$(( $(date -d "$(systemctl --user show marshal-daemon -p ActiveEnterTimestamp --value)" +%s) * 1000 ))
log "restarted at $(date -d @$((restart_ms/1000)) +%T), PID $pid0 -> $(systemctl --user show marshal-daemon -p MainPID --value)"
for _ in $(seq 1 20); do [ "$(roster_field "$SID" bound)" = True ] && break; sleep 3; done
read -r _ r1 <<<"$(token_state "$tok")"
[ "$r1" = - ] || fail "token taken at $r1 with no hook running"

type_into "$W" "Tell me exactly what any terminal messages you just received say, then answer ok."
sleep 30
read -r _ r2 <<<"$(token_state "$tok")"
[ "$r2" != - ] || fail "token not taken after the prompt"
[ "$r2" -gt "$restart_ms" ] || fail "token taken at $r2, before the restart"
n=$(q "select count(*) from myko_events where item_type='MessageRead' and change_type='SET' and event->'item'->>'messageId'='$mid'")
[ "$n" = 1 ] || fail "token has $n MessageReads, not 1"
[ "$(roster_field "$SID" nickname)" = "$NICK" ] || fail "nickname changed"
printf '%s\tPASS\tmessage %s unread across the restart at %s, taken at %s, once; nickname %s kept\n' \
  "$(now_ms)" "$mid" "$restart_ms" "$r2" "$NICK" >> "$OUT"
log "PASS: $mid unread across the restart, taken once after it, nickname kept"
