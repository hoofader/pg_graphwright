# pg_graphwright

A knowledge-graph index for Postgres, where each user sees only the part of the graph derived from rows they are allowed to read.

pg_graphwright builds an entity graph (people, places, things and the relationships between them) from the documents in your tables, and keeps it as Postgres-managed state. The position no other system takes: **a graph element's visibility follows the row-level security of its source rows.** If a user cannot read the note that a fact came from, that user does not see the fact. There is no second access-control system to keep in sync with your data; the extension delegates to Postgres RLS.

This is the Postgres-native sibling of [graphwright](https://github.com/hoofader/graphwright) (the storage-agnostic TypeScript core) and [graphwright-onnx](https://github.com/hoofader/graphwright-onnx) (the no-LLM extraction backend). The planning logic lives there; this repo is where it becomes an index.

## Status

Early. The data model, the RLS-aware query surface, a real index access method, and live maintenance are in place and proven end to end. `CREATE INDEX ... USING graphwright (body)` builds the graph; a change trigger keeps it current as rows are inserted, updated, and deleted; the accessors filter it per user against the source table's RLS. Extraction is still a deterministic stub (tokenize a row, co-mention edges). The pieces still to come:

- a real extraction seam (a local LLM / GLiNER via graphwright-onnx, judged by a larger model),
- the resolution cascade (phonetic, fuzzy, embedding) ported from the graphwright core,
- a human-in-the-loop review queue (proposals an operator accepts or rejects),
- locking the catalog down so the accessors are the only door.

What is already proven is the part nobody else ships: row-derived graph visibility.

## Try it

```sql
CREATE EXTENSION pg_graphwright;

-- A table of documents behind an RLS policy.
CREATE TABLE notes (id int PRIMARY KEY, owner text, body text);
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
CREATE POLICY owner_can_read ON notes USING (owner = current_user);

INSERT INTO notes VALUES
  (1, 'amir', 'Sara Tehran'),
  (2, 'nadia', 'Sara Berlin');

-- Build the knowledge-graph index over the body column.
CREATE INDEX notes_kg ON notes USING graphwright (body);

-- amir sees only the graph from row 1; nadia only from row 2.
SET ROLE amir;
SELECT * FROM graphwright.edges('notes');
```

`graphwright.watch(table, text_col, pk_col)` + `graphwright.reindex(id)` are also exposed for building the graph without an index (with a primary-key column as provenance instead of `ctid`).

## Edge visibility

An edge can be supported by more than one source row. Two rules, set per watch (default `union`):

- **`union`**: the edge is visible if the user can read any one supporting row. Safe for directly-extracted edges, because a single row already justifies the fact to that user.
- **`intersection`**: the edge is visible only if the user can read every supporting row.

```sql
UPDATE graphwright.watch SET visibility = 'intersection'
  WHERE source_table = 'notes'::regclass;
```

## Live maintenance

`CREATE INDEX` installs a trigger that records every changed row into a change queue (`graphwright.dirty`). Draining the queue applies the changes incrementally, dropping any entity or edge that loses its last supporting row. Drain it whichever way fits:

```sql
SELECT graphwright.maintain();           -- drain every watch now (e.g. from pg_cron)
SELECT graphwright.process_dirty(1);     -- or one watch
```

For a worker that drains automatically, preload the library and name the database:

```
# postgresql.conf
shared_preload_libraries = 'pg_graphwright'
graphwright.database = 'mydb'
```

Extraction is a fast stub today, so draining is cheap. When it becomes LLM-backed, the queue is what keeps the work off the writing transaction.

## Build

```bash
cargo pgrx run pg17      # build, install, open a psql
cargo pgrx test pg17     # run the regression tests
```

Requires the pgrx toolchain (`cargo install cargo-pgrx`, then `cargo pgrx init`). Built against `pgrx 0.18`.

## License

AGPL-3.0-only. See [LICENSE](LICENSE).
