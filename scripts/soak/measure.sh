#!/usr/bin/env bash
# MAR-75: take MAR-73's measures for both daemons over the soak window, plus the
# kit's own canary and restart results. The journal script and the SQL are
# read from docs/reliability-vs-muster.md at run time, so there's one copy.
# Read-only throughout.
#   measure.sh [hours]      defaults to 24
set -euo pipefail
. "$(dirname "$0")/lib.sh"
HOURS=${1:-24}
OUTD=$STATE/measures-$(date +%Y%m%dT%H%M%S)
mkdir -p "$OUTD"
python3 -I - "$KIT_ROOT/docs/reliability-vs-muster.md" "$OUTD" <<'EOF'
import re, sys
doc, out = open(sys.argv[1]).read(), sys.argv[2]
open(f"{out}/reliability.sql", "w").write(re.search(r"```sql\n(.*?)```", doc, re.S).group(1))
open(f"{out}/journal.sh", "w").write(re.findall(r"```sh\n(.*?)```", doc, re.S)[0])
EOF

for pair in muster-daemon:muster marshal-daemon:marshal; do
  u=${pair%%:*} db=${pair##*:}
  {
    bash "$OUTD/journal.sh" "$u" "$HOURS hours ago"
    R=$(journalctl --user -u "$u" --since "$HOURS hours ago" -o short-unix --no-pager \
          | awk '/Started /{printf "%s%d", (n++?",":""), $1*1000}')   # awk, not grep: no restart is no failure
    PGOPTIONS='-c default_transaction_read_only=on' \
      psql -h "$PG_SOCKET" -d "$db" -X -A -v hours="$HOURS" -v restarts="'{$R}'" -f "$OUTD/reliability.sql"
  } > "$OUTD/$db.txt" 2>&1
  log "$db: $OUTD/$db.txt"
done

# The canary: how many rounds, taken and woke within 60 s, and the time to take.
if [ -s "$STATE/canary.tsv" ]; then
  python3 -I - "$STATE/canary.tsv" > "$OUTD/canary.txt" <<'EOF'
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1]), delimiter="\t"))
t = sorted(float(r["taken_s"]) for r in rows if r["taken_s"] != "-")
q = lambda p: t[min(len(t) - 1, int(p * len(t)))] if t else None
print(f"canary sends={len(rows)} taken_within_60s={len(t)} woke_within_60s={sum(r['woke'] == 'yes' for r in rows)}")
print(f"time to take p50={q(0.5)} p95={q(0.95)} max={max(t) if t else None} (s)")
EOF
fi
[ -s "$STATE/sends.tsv" ] && echo "messages sent by drive.sh: $(($(wc -l < "$STATE/sends.tsv") - 1))" >> "$OUTD/canary.txt"
[ -s "$STATE/restart.tsv" ] && cp "$STATE/restart.tsv" "$OUTD/"
log "measures in $OUTD"
cat "$OUTD"/canary.txt 2>/dev/null || true
