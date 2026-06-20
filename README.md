# pg_graphwright

A knowledge-graph index for Postgres, where each user sees only the part of the graph derived from rows they are allowed to read.

pg_graphwright builds an entity graph (people, places, things and the relationships between them) from the documents in your tables, and keeps it as Postgres-managed state. The position no other system takes: **a graph element's visibility follows the row-level security of its source rows.** If a user cannot read the note that a fact came from, that user does not see the fact. There is no second access-control system to keep in sync with your data; the extension delegates to Postgres RLS.

This is the Postgres-native sibling of [graphwright](https://github.com/hoofader/graphwright) (the storage-agnostic TypeScript core) and [graphwright-onnx](https://github.com/hoofader/graphwright-onnx) (the no-LLM extraction backend). The planning logic lives there; this repo is where it becomes an index.

## Status

Early. This is milestone 1a: the data model and the RLS-aware query surface, proven end to end. Extraction is a deterministic stub (tokenize a row, co-mention edges), wired through a manual `reindex` rather than a live index. The pieces still to come:

- the real index access method, so `CREATE INDEX ... USING graphwright (body)` drives extraction on row change,
- a background worker for incremental maintenance,
- a real extraction seam (a local LLM / GLiNER via graphwright-onnx, judged by a larger model),
- the resolution cascade (phonetic, fuzzy, embedding) ported from the graphwright core,
- a human-in-the-loop review queue (proposals an operator accepts or rejects).

What milestone 1a does prove is the part nobody else ships: row-derived graph visibility.

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

-- Register the text column and build the graph over every row.
SELECT graphwright.watch('notes', 'body', 'id');
SELECT graphwright.reindex(1);

-- amir sees only the graph from row 1; nadia only from row 2.
SET ROLE amir;
SELECT * FROM graphwright.edges('notes');
```

## Edge visibility

An edge can be supported by more than one source row. Two rules, set per watch (default `union`):

- **`union`**: the edge is visible if the user can read any one supporting row. Safe for directly-extracted edges, because a single row already justifies the fact to that user.
- **`intersection`**: the edge is visible only if the user can read every supporting row.

```sql
UPDATE graphwright.watch SET visibility = 'intersection'
  WHERE source_table = 'notes'::regclass;
```

## Build

```bash
cargo pgrx run pg17      # build, install, open a psql
cargo pgrx test pg17     # run the regression tests
```

Requires the pgrx toolchain (`cargo install cargo-pgrx`, then `cargo pgrx init`). Built against `pgrx 0.18`.

## License

Apache-2.0
