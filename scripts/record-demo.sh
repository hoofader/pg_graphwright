#!/usr/bin/env bash
# Record the diary demo: same table, same query, two users, two graphs.
#
# This produces the asciinema cast behind the launch GIF. It needs the
# extension installed in a Postgres you can reach with `psql` (it honors the
# standard PG* env vars, or a conninfo in $PSQL_CONN).
#
# One-time setup (a throwaway database is fine):
#
#   cargo pgrx install --no-default-features --features pg18 \
#       --pg-config "$(cargo pgrx info pg-config pg18)"
#   createdb graphwright_demo
#   PGDATABASE=graphwright_demo ./scripts/record-demo.sh setup
#
# Record the reveal (this is the 15 seconds worth keeping):
#
#   asciinema rec -c 'PGDATABASE=graphwright_demo ./scripts/record-demo.sh reveal' \
#       examples/diary/demo.cast
#   agg examples/diary/demo.cast examples/diary/demo.gif   # cast -> gif (asciinema/agg)
#
set -euo pipefail
cd "$(dirname "$0")/.."

# psql wrapper: applies the connection and the flags every step wants.
pg() { command psql ${PSQL_CONN:+"$PSQL_CONN"} -v ON_ERROR_STOP=1 -P pager=off "$@"; }

setup() {
  pg -q -f examples/diary/schema.sql >/dev/null
  pg -q >/dev/null <<'SQL'
DROP ROLE IF EXISTS emma; DROP ROLE IF EXISTS jack;
CREATE ROLE emma; CREATE ROLE jack;
DELETE FROM diary;
SET ROLE emma;
INSERT INTO diary (entry) VALUES
  ('Another long day at work. I left early to meet Sara at the coffee place we always go to. She brought Tom along, and the three of us talked for hours.'),
  ('Quiet morning at home. Sarah called while I was making tea, and we ended up talking for almost an hour about her sister. She keeps saying I should finally meet Lucy.'),
  ('After work I took a long walk with Tom. He has been weighing a new job and was not sure what to do. We ran into Lucy on the way back, and the three of us got dinner.');
RESET ROLE;
SET ROLE jack;
INSERT INTO diary (entry) VALUES
  ('Busy week. Sara from the office stopped by my desk to go over some numbers, and we spent twenty minutes complaining about the new system. After that I grabbed lunch with Daniel.'),
  ('I stayed in tonight. Daniel sent a long voice message about his weekend trip. He sounded tired but in good spirits.');
RESET ROLE;
SELECT graphwright.maintain();
SQL
  echo "seeded. record with:"
  echo "  asciinema rec -c '$0 reveal' examples/diary/demo.cast"
}

GREEN=$'\033[1;32m'; DIM=$'\033[2m'; RST=$'\033[0m'
# "Type" the line at a psql-ish prompt, then run it.
step() {
  local sql="$1" pause="${2:-1.4}" i
  printf '%sdiary=#%s ' "$GREEN" "$RST"
  for ((i = 0; i < ${#sql}; i++)); do printf '%s' "${sql:i:1}"; sleep "${TYPE_DELAY:-0.015}"; done
  printf '\n'
  pg -c "$sql"
  sleep "$pause"
}
say() { printf '\n%s-- %s%s\n' "$DIM" "$1" "$RST"; sleep 1.2; }

reveal() {
  clear
  say "Same table, same query. Row-level security decides each user's graph."
  step "SET ROLE emma; SELECT person FROM my_people ORDER BY person;"
  step "SET ROLE jack; SELECT person FROM my_people ORDER BY person;"
  say "A direct catalog read is filtered the same way. No back door."
  step "SET ROLE jack; SELECT count(*) AS jack_can_see FROM graphwright.entity;"
  say "Emma confirms Sara = Sarah. Durable and reversible."
  step "SELECT graphwright.merge('diary','Sara','Sarah'); SELECT graphwright.maintain();" 1.0
  step "SET ROLE emma; SELECT person FROM my_people ORDER BY person;" 2.0
}

case "${1:-reveal}" in
  setup) setup ;;
  reveal) reveal ;;
  *)
    echo "usage: $0 {setup|reveal}" >&2
    exit 1
    ;;
esac
