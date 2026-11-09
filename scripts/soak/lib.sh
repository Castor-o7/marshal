# Shared settings and helpers for the MAR-75 soak kit. Sourced, not run.
# Every setting can be overridden from the environment.

SOAK_DIR=${SOAK_DIR:-$HOME/src/marshal-soak}      # test sessions' directories and the kit's state
SOAK_TMUX=${SOAK_TMUX:-marshal-soak}               # the tmux session holding one window per test session
SOAK_RESULTS=${SOAK_RESULTS:-$HOME/.local/state/marshal-soak}   # teardown keeps the state here
MARSHAL_DAEMON_ADDRESS=${MARSHAL_DAEMON_ADDRESS:-ws://127.0.0.1:6155}
SHIM=${SHIM:-$HOME/.cargo/bin/marshal-shim}
PG_SOCKET=${PG_SOCKET:-/run/user/$(id -u)/marshal-pg}
export MARSHAL_DAEMON_ADDRESS

STATE=$SOAK_DIR/state
SESSIONS=$STATE/sessions.tsv                       # i, dir, nickname, session_id
KIT_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

now_ms() { date +%s%3N; }
log() { printf '%s %s\n' "$(date +%T)" "$*" >&2; }

# A read-only query on the marshal database: every transaction is read-only,
# so even a temporary object is refused.
q() {
  PGOPTIONS='-c default_transaction_read_only=on' \
    psql -h "$PG_SOCKET" -d "${2:-marshal}" -X -A -t -c "$1"
}

# The roster as JSON, from Marshal only.
roster() { "$SHIM" roster; }

# One roster field for a session id: wake, bound, waiting_until or cwd.
roster_field() {
  roster | python3 -I -c '
import json, sys
sid, field = sys.argv[1], sys.argv[2]
for s in json.load(sys.stdin)["sessions"]:
    if s.get("session_id") == sid:
        rec, dec = s.get("recorded") or {}, s.get("declared") or {}
        print({"wake": s.get("wake"), "bound": rec.get("bound_to_connection"),
               "waiting_until": rec.get("waiting_until"), "cwd": dec.get("cwd"),
               "nickname": s.get("nickname")}[field])
        break
' "$1" "$2"
}

# The message id and first read time (ms, or empty) of the message whose body ends with $1.
token_state() {
  local id
  id=$(q "select item_id from myko_events where item_type='Message' and change_type='SET'
          and event->'item'->>'body' like '%$1' order by id desc limit 1")
  [ -n "$id" ] || { echo "- -"; return; }
  local read
  read=$(q "select min((event->'item'->>'readAt')::bigint) from myko_events
            where item_type='MessageRead' and change_type='SET' and event->'item'->>'messageId'='$id'")
  echo "$id ${read:--}"
}

# Send text to a test session's window, then Enter separately: a long line
# sent with its Enter can sit unsubmitted (MAR-70 run 1).
type_into() {
  tmux send-keys -t "$SOAK_TMUX:$1" -l "$2"
  sleep 1
  tmux send-keys -t "$SOAK_TMUX:$1" Enter
}

# The PID of the claude process running in a test session's window.
claude_pid() {
  local pane
  pane=$(tmux list-panes -t "$SOAK_TMUX:$1" -F '#{pane_pid}' | head -1)
  if [ "$(ps -o comm= -p "$pane")" = claude ]; then echo "$pane"; else pgrep -P "$pane" -x claude; fi
}

need_sessions() { [ -s "$SESSIONS" ] || { log "no $SESSIONS: run setup.sh first"; exit 1; }; }
