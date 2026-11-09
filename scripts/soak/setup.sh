#!/usr/bin/env bash
# MAR-75: start N test sessions wired only to Marshal, each in its own directory
# with the project-local wiring of MAR-59's kit (docs/cutover-test-session.md),
# one tmux window each. Changes nothing user-wide.
#   setup.sh [N]      N defaults to 3
set -euo pipefail
. "$(dirname "$0")/lib.sh"
N=${1:-3}

[ ! -e "$SOAK_DIR" ] || { log "$SOAK_DIR exists: run teardown.sh first"; exit 1; }
! tmux has-session -t "$SOAK_TMUX" 2>/dev/null || { log "tmux session $SOAK_TMUX exists: run teardown.sh first"; exit 1; }
others=$(roster | python3 -I -c 'import json,sys; print(len(json.load(sys.stdin)["sessions"]))')
log "Marshal's roster has $others session(s) before setup"

mkdir -p "$STATE"
: > "$SESSIONS"
for i in $(seq 1 "$N"); do
  d=$SOAK_DIR/s$i
  mkdir -p "$d/.claude"
  # The wiring is copied from the kit's own JSON blocks, so the two can't drift.
  python3 -I - "$KIT_ROOT/docs/cutover-test-session.md" "$d" <<'EOF'
import json, re, sys
doc, d = sys.argv[1], sys.argv[2]
blocks = re.findall(r"```json\n(.*?)```", open(doc).read(), re.S)
settings = next(b for b in blocks if '"hooks"' in b)
mcp = next(b for b in blocks if '"mcpServers"' in b)
json.loads(settings); json.loads(mcp)
open(f"{d}/.claude/settings.json", "w").write(settings)
open(f"{d}/.mcp.json", "w").write(mcp)
EOF
  (cd "$d" && "$SHIM" doctor >"$STATE/doctor-s$i.txt" 2>&1) || { cat "$STATE/doctor-s$i.txt"; log "doctor failed for s$i"; exit 1; }
  cmd="cd '$d' && exec env MARSHAL_DAEMON_ADDRESS=$MARSHAL_DAEMON_ADDRESS claude --setting-sources project --strict-mcp-config --mcp-config .mcp.json"
  if [ "$i" = 1 ]; then
    tmux new-session -d -s "$SOAK_TMUX" -n s1 -x 160 -y 50 "$cmd"
  else
    tmux new-window -d -t "$SOAK_TMUX" -n "s$i" "$cmd"
  fi
done

sleep 10
prompt="This is a Marshal soak test session (MAR-75). You will get short test messages from a terminal that can't receive replies. For each one, answer with the single word ok, and don't use any tools. Now answer ready."
for i in $(seq 1 "$N"); do type_into "s$i" "$prompt"; done

# Wait for each session's roster row, idle with a waiter held.
for i in $(seq 1 "$N"); do
  d=$SOAK_DIR/s$i row=""
  for _ in $(seq 1 30); do
    row=$(roster | python3 -I -c '
import json, sys
for s in json.load(sys.stdin)["sessions"]:
    if (s.get("declared") or {}).get("cwd") == sys.argv[1] and s.get("wake") == "waiting":
        print(s["nickname"], s["session_id"]); break
' "$d")
    [ -n "$row" ] && break
    sleep 3
  done
  [ -n "$row" ] || { log "s$i didn't reach the roster idle with a waiter; see tmux window $SOAK_TMUX:s$i"; exit 1; }
  sid=${row##* }
  for _ in $(seq 1 30); do   # its answer to the first prompt, and a waiter held again after it
    tmux capture-pane -p -t "$SOAK_TMUX:s$i" | grep -q '● ready' && [ "$(roster_field "$sid" wake)" = waiting ] && break
    sleep 3
  done
  tmux capture-pane -p -t "$SOAK_TMUX:s$i" | grep -q '● ready' || { log "s$i didn't answer ready; see tmux window $SOAK_TMUX:s$i"; exit 1; }
  printf '%s\t%s\t%s\n' "s$i" "$d" "${row// /$'\t'}" >> "$SESSIONS"
  log "s$i is ${row%% *}"
done
log "ready: $(cut -f3 "$SESSIONS" | paste -sd' ')"
