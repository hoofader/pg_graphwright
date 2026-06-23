#!/usr/bin/env bash
# Record an example demo as an asciinema cast, for the README GIFs.
#
#   record-demo.sh <diary|enterprise> <setup|reveal>
#
# It needs the extension installed in a Postgres you can reach with `psql`
# (it honors the standard PG* env vars, or a conninfo in $PSQL_CONN).
#
# One-time setup (a throwaway database is fine):
#
#   cargo pgrx install --no-default-features --features pg18 \
#       --pg-config "$(cargo pgrx info pg-config pg18)"
#   createdb graphwright_demo
#   PGDATABASE=graphwright_demo ./scripts/record-demo.sh diary setup
#
# Record the reveal (this is the ~15 seconds worth keeping):
#
#   asciinema rec -c 'PGDATABASE=graphwright_demo ./scripts/record-demo.sh diary reveal' \
#       examples/diary/demo.cast
#   agg examples/diary/demo.cast examples/diary/demo.gif   # cast -> gif (asciinema/agg)
#
# vhs is the obvious tool but needs a headless browser + localhost; asciinema
# plus agg renders without either, which is why this is the documented path.
set -euo pipefail
cd "$(dirname "$0")/.."

# psql wrapper: applies the connection and the flags every step wants.
pg() { command psql ${PSQL_CONN:+"$PSQL_CONN"} -v ON_ERROR_STOP=1 -P pager=off "$@"; }

GREEN=$'\033[1;32m'; DIM=$'\033[2m'; RST=$'\033[0m'
PROMPT="psql"
# "Type" the line at a psql-ish prompt, then run it.
step() {
  local sql="$1" pause="${2:-1.4}" i
  printf '%s%s=#%s ' "$GREEN" "$PROMPT" "$RST"
  for ((i = 0; i < ${#sql}; i++)); do printf '%s' "${sql:i:1}"; sleep "${TYPE_DELAY:-0.015}"; done
  printf '\n'
  pg -c "$sql"
  sleep "$pause"
}
say() { printf '\n%s-- %s%s\n' "$DIM" "$1" "$RST"; sleep 1.2; }

diary_setup() {
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
  echo "seeded diary."
}

diary_reveal() {
  PROMPT="diary"
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

enterprise_setup() {
  pg -q -f examples/enterprise/schema.sql >/dev/null
  pg -q >/dev/null <<'SQL'
DROP ROLE IF EXISTS kimi, ravi, joe, mia, dana;
CREATE ROLE kimi LOGIN; CREATE ROLE ravi LOGIN; CREATE ROLE joe LOGIN;
CREATE ROLE mia LOGIN;  CREATE ROLE dana LOGIN;
GRANT engineering TO kimi, ravi;
DELETE FROM docs;
INSERT INTO docs (dept, owner, title, body) VALUES
  ('engineering', 'kimi', 'Sprint notes', 'Kimi paired with Ravi on the Stark integration all week.'),
  ('engineering', 'ravi', 'Pipeline debugging', 'Ravi met Tessa from Stark to debug the data pipeline.'),
  ('marketing', 'dana', 'Campaign recap', 'Dana shipped the Initech campaign with help from Priya.'),
  ('sales', 'joe', 'Globex account', 'Joe finally closed the Globex account. Nadia at Globex signed the contract.'),
  ('sales', 'mia', 'Umbrella pursuit', 'Mia is courting Umbrella. Victor at Umbrella is the champion.');
SELECT graphwright.maintain();
SQL
  echo "seeded enterprise."
}

# Each employee's graph as one line, so the before/after of a share is a clean diff.
ent_q() { echo "SET ROLE $1; SELECT string_agg(who, ', ' ORDER BY who) AS $2 FROM my_people;"; }

enterprise_reveal() {
  PROMPT="acme"
  clear
  say "One company. Each employee's graph is the documents their access allows."
  step "$(ent_q kimi engineer_kimi)"
  step "$(ent_q joe sales_joe)"
  say "Joe shares the Globex account doc with Kimi, an engineer."
  step "SET ROLE joe; INSERT INTO doc_shares(doc_id, shared_with) SELECT id, 'kimi' FROM docs WHERE owner='joe';" 1.0
  say "Kimi's graph now has Globex, Joe, Nadia. No graphwright.maintain() ran."
  step "$(ent_q kimi kimi_after_share)"
  say "Joe unshares it, and it leaves her graph. Still no rebuild."
  step "SET ROLE joe; DELETE FROM doc_shares WHERE shared_with='kimi';" 1.0
  step "$(ent_q kimi kimi_after_unshare)" 2.0
}

case "${1:-}/${2:-}" in
  diary/setup) diary_setup ;;
  diary/reveal) diary_reveal ;;
  enterprise/setup) enterprise_setup ;;
  enterprise/reveal) enterprise_reveal ;;
  *)
    echo "usage: $0 <diary|enterprise> <setup|reveal>" >&2
    exit 1
    ;;
esac
