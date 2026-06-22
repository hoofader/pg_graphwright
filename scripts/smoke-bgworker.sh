#!/usr/bin/env bash
# Smoke-test the background maintenance worker end to end.
#
# It cannot be a #[pg_test]: the worker needs shared_preload_libraries and a
# restart, runs in its own backend, and only ever sees committed data. So
# this is a standalone script you run by hand:
#
#   ./scripts/smoke-bgworker.sh
#
# It builds a throwaway cluster from the same Postgres the tests use, sets up
# a watched, indexed table, then points the worker at the database and proves
# the graph gets built with no explicit graphwright.maintain() call.
set -euo pipefail

FEATURE="${FEATURE:-pg17}"
PORT="${PORT:-54329}"
DBNAME=graphwright_smoke

# Use $PG_CONFIG if it points at a real binary, else find a Postgres 17 one.
if [ -z "${PG_CONFIG:-}" ] || [ ! -x "${PG_CONFIG:-}" ]; then
  PG_CONFIG=""
  for cand in \
    "$(cargo pgrx info pg-config "$FEATURE" 2>/dev/null || true)" \
    /opt/homebrew/opt/postgresql@17/bin/pg_config \
    /usr/local/opt/postgresql@17/bin/pg_config \
    "$(command -v pg_config || true)"; do
    if [ -n "$cand" ] && [ -x "$cand" ]; then PG_CONFIG="$cand"; break; fi
  done
fi
if [ -z "${PG_CONFIG:-}" ] || [ ! -x "$PG_CONFIG" ]; then
  echo "no usable pg_config found; set PG_CONFIG to your Postgres 17 pg_config" >&2
  exit 1
fi

BINDIR="$("$PG_CONFIG" --bindir)"
TMP="$(mktemp -d)"
DATADIR="$TMP/data"
cd "$(dirname "$0")/.."

cleanup() {
  "$BINDIR/pg_ctl" -D "$DATADIR" stop -m immediate >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

psql() { "$BINDIR/psql" -v ON_ERROR_STOP=1 -qtAX -U postgres -h localhost -p "$PORT" "$@"; }

echo "==> install the extension into $("$PG_CONFIG" --version)"
cargo pgrx install --no-default-features --features "$FEATURE" --pg-config "$PG_CONFIG" >/dev/null

echo "==> initdb + start (worker idle: graphwright.database unset)"
"$BINDIR/initdb" -D "$DATADIR" -U postgres --no-sync >/dev/null
cat >>"$DATADIR/postgresql.conf" <<CONF
port = $PORT
listen_addresses = 'localhost'
shared_preload_libraries = 'pg_graphwright'
CONF
"$BINDIR/pg_ctl" -D "$DATADIR" -l "$TMP/log" -w start >/dev/null

echo "==> create db, extension, a watched index, and data"
"$BINDIR/createdb" -U postgres -h localhost -p "$PORT" "$DBNAME"
psql -d "$DBNAME" >/dev/null <<'SQL'
CREATE EXTENSION pg_graphwright;
CREATE TABLE notes (id int PRIMARY KEY, body text);
INSERT INTO notes VALUES (1, 'Sara Tehran'), (2, 'Reza Berlin');
CREATE INDEX notes_kg ON notes USING graphwright (body);
SQL

# Async-on-create: the graph is empty until something runs maintain().
pre=$(psql -d "$DBNAME" -c "SELECT count(*) FROM graphwright.entities('notes')")
echo "    entities before the worker runs: ${pre:-0}"

echo "==> point the worker at the db and restart so it picks it up"
psql -d postgres -c "ALTER SYSTEM SET graphwright.database = '$DBNAME'" >/dev/null
"$BINDIR/pg_ctl" -D "$DATADIR" -w restart >/dev/null

echo "==> wait for a worker tick (interval is ~10s)"
ok=
for i in $(seq 1 6); do
  sleep 5
  n=$(psql -d "$DBNAME" -c "SELECT count(*) FROM graphwright.entities('notes')")
  echo "    attempt $i: entities=${n:-0}"
  if [ "${n:-0}" -ge 4 ]; then ok=1; break; fi
done

if [ -n "$ok" ]; then
  echo "PASS: the worker built the graph with no explicit maintain() call."
else
  echo "FAIL: graph not built within the wait window. graphwright log lines:"
  grep -i graphwright "$TMP/log" || true
  exit 1
fi
