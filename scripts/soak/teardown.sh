#!/usr/bin/env bash
# MAR-75: end the test sessions, keep the kit's state and measures under
# $SOAK_RESULTS/<time>, remove $SOAK_DIR, and wait for Marshal's roster to drop
# the sessions' rows. Nothing outside $SOAK_DIR was changed.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
mapfile -t SIDS < <(cut -f4 "$SESSIONS" 2>/dev/null)

if tmux has-session -t "$SOAK_TMUX" 2>/dev/null; then
  for w in $(tmux list-windows -t "$SOAK_TMUX" -F '#{window_name}'); do
    tmux send-keys -t "$SOAK_TMUX:$w" Escape; tmux send-keys -t "$SOAK_TMUX:$w" -l '/exit'; tmux send-keys -t "$SOAK_TMUX:$w" Enter
  done
  sleep 5
  tmux kill-session -t "$SOAK_TMUX" 2>/dev/null
  log "tmux session $SOAK_TMUX ended"
fi
if [ -d "$STATE" ]; then
  keep=$SOAK_RESULTS/$(date +%Y%m%dT%H%M%S)
  mkdir -p "$keep" && cp -r "$STATE"/. "$keep"/ && log "state kept in $keep"
fi
# Only ever remove a directory setup.sh made: it holds setup's marker file.
if [ -e "$SOAK_DIR" ] && [ ! -f "$MARKER" ]; then
  log "$SOAK_DIR has no $(basename "$MARKER"), so setup.sh didn't make it; not removing it"; exit 1
fi
rm -rf "$SOAK_DIR" && log "removed $SOAK_DIR"
left=0
for p in $(pgrep -x claude; pgrep -f 'marshal-shim'); do
  case "$(readlink "/proc/$p/cwd" 2>/dev/null)" in "$SOAK_DIR"*) left=$((left + 1));; esac
done
[ "$left" = 0 ] || log "warning: $left process(es) still run in $SOAK_DIR"

# Rows go once their connection has been gone for STALE_AFTER (60 s).
for _ in $(seq 1 40); do
  n=0; for s in "${SIDS[@]}"; do [ -n "$(roster_field "$s" nickname)" ] && n=$((n + 1)); done
  [ "$n" = 0 ] && { log "roster: no test sessions left"; exit 0; }
  sleep 5
done
log "roster still lists $n test session(s) after 200 s"; exit 1
