# pg_graphwright

A knowledge-graph index for Postgres, where each user sees only the part of the graph derived from rows they are allowed to read.

pg_graphwright builds an entity graph (people, places, things and the relationships between them) from the documents in your tables, and keeps it as Postgres-managed state. The position no other system takes: **a graph element's visibility follows the row-level security of its source rows.** If a user cannot read the note that a fact came from, that user does not see the fact. There is no second access-control system to keep in sync with your data; the extension delegates to Postgres RLS.

This is the Postgres-native sibling of [graphwright](https://github.com/hoofader/graphwright) (the storage-agnostic TypeScript core) and [graphwright-onnx](https://github.com/hoofader/graphwright-onnx) (the no-LLM extraction backend). The planning logic lives there; this repo is where it becomes an index.

## Status

Early, but the storage model is now the Postgres-native one. `CREATE INDEX ... USING graphwright (body)` stores each row's extraction in the **index relation's own pages** (WAL-logged through generic WAL, like pg_search), so it is transactional with the heap and travels with physical replication. `aminsert` writes only a tiny marker on a write, and `CREATE INDEX` itself only marks rows; the extractor, judge, and resolved graph build on the next `graphwright.maintain()` (or background-worker) tick, which runs as the extension owner so it sees every row. A slow model never blocks a write. `ambulkdelete` reclaims deleted rows' records on vacuum. The cross-row resolved graph (entities/edges) is derived from index storage into catalog tables that carry **row-level security** so a graph row is visible exactly when the source row(s) behind it are; the accessors and a direct catalog read are filtered the same way. Extraction and judging are pluggable seams (`graphwright.extractor`, `graphwright.judge`), defaulting to a built-in tokenizer and no judge. Resolution folds entity surfaces on a normalized key (Arabic/Persian variants meet), and auto-merges distinctive cross-script phonetic and 3-gram fuzzy matches (both gated by entropy). It never waits: every merge is recorded in a durable, reversible decision log a human edits after the fact (SAGA-style, down to splitting a single mention out of an exact fold), and `graphwright.proposals()` shows the matches the gate left for review. The pieces still to come:

- full NFKC in the resolver (the last cascade stage),
- RLS on `edge_support` (today it is unprotected; it holds only opaque `edge_id`/`source_pk`, no names).

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

## How the visibility is enforced

The catalog tables carry their own row-level security. A `SECURITY INVOKER` function `graphwright._pk_visible(watch_id, source_pk)` runs an `EXISTS` against the source table, so the source's RLS decides as the calling user; the `entity`/`mention`/`edge`/`entity_phonetic`/`decision`/`mention_override` policies build on it. So a direct `SELECT * FROM graphwright.entity` is filtered the same as the accessor, and the accessor is no privileged back door:

```sql
SET ROLE analyst_without_access;
SELECT count(*) FROM graphwright.entity;   -- only entities from rows analyst can read
```

Maintenance runs as the extension owner (which bypasses RLS, so the graph is built over every row) and the maintenance and review functions (`maintain`, `reindex`, `gc`, `merge`/`split`/`unmerge`, `split_mention`/`unsplit_mention`, `index_dump`) have `EXECUTE` revoked from `PUBLIC`. Grant those to your operator and reviewer roles. The owner must be able to read every source row (a superuser, a `BYPASSRLS` role, or the table owner) for extraction to be complete.

## Live maintenance

Writes update index storage immediately (`aminsert`, in the writing transaction). The resolved graph is refreshed from that storage by `maintain()`:

```sql
SELECT graphwright.maintain();   -- re-resolve every graphwright index (e.g. from pg_cron)
```

For a worker that runs it automatically, preload the library and name the database:

```
# postgresql.conf
shared_preload_libraries = 'pg_graphwright'
graphwright.database = 'mydb'
```

Splitting the synchronous storage write from the async resolve is deliberate: extraction is a fast stub today, but when it becomes LLM-backed, the per-row tokens are still captured transactionally while the expensive resolution stays off the writing transaction.

There is also a no-index path (`graphwright.watch(table, text_col, pk_col)` + `graphwright.reindex(id)`) that builds the graph straight from source rows, with a trigger-fed queue (`graphwright.process_dirty(id)`) for incremental updates. Use it when you want the graph without `CREATE INDEX`.

## Extraction

What counts as an entity is a pluggable seam, so the extension stays model-agnostic (the way graphwright's core treats the LLM as injected). Point `graphwright.extractor` at a SQL function `f(text) -> text[]`; leave it empty for the built-in tokenizer.

```sql
-- a toy extractor: capitalized words are entities
CREATE FUNCTION caps(doc text) RETURNS text[] LANGUAGE sql AS $$
  SELECT array_agg(w) FROM regexp_split_to_table(doc, '\s+') AS w WHERE w ~ '^[A-Z]'
$$;
SET graphwright.extractor = 'caps';
```

The function can wrap anything: a regex NER, an LLM gateway over `pg_net`, or GLiNER through [graphwright-onnx](https://github.com/hoofader/graphwright-onnx) called from pl/python or an HTTP service. It runs asynchronously (the maintenance pass), so a slow model is fine; a write only records a marker.

A second seam validates the result. `graphwright.judge` names a function `j(text, text[]) -> text[]` (a larger model) that trims or vets the extractor's mentions before they reach the graph:

```sql
CREATE FUNCTION vet(doc text, mentions text[]) RETURNS text[] LANGUAGE sql AS $$
  SELECT array_agg(m) FROM unnest(mentions) AS m WHERE m <> 'secret'
$$;
SET graphwright.judge = 'vet';
```

This is the "AI output is never canon" step: the small model proposes mentions, the larger model disposes. A judge error or NULL leaves the extractor's output unchanged.

## Resolution

A mention's surface resolves to an entity by **exact match on a normalized key** (ported from the graphwright core): Arabic vs Persian yeh/kaf, alef variants, diacritics, tatweel, ZWNJ joins, case, and surrounding punctuation all fold, so `علي` and `علی` are one entity.

Beyond exact, two lanes auto-merge when the name is distinctive enough (an entropy gate). A **cross-script phonetic match**: `Khashayar` and `خشایار` become one entity. A **3-gram fuzzy match** (Jaccard >= 0.82): a consonant typo forks the phonetic skeleton but barely moves the shingle overlap, so this catches spellings phonetic misses. Short names like `Ali` / `علی` stay apart and show up as proposals:

```sql
SELECT * FROM graphwright.proposals('notes');  -- gated-out candidates to review
```

### Reviewing decisions

Nothing waits for a human. Every identity decision is replayed from a durable log on each re-resolve, and you correct it after the fact, SAGA-style:

```sql
SELECT graphwright.split('notes', 'Khashayar', 'خشایار');  -- veto an auto-merge
SELECT graphwright.merge('notes', 'Ali', 'علی');           -- force a merge
SELECT graphwright.unmerge('notes', 'Ali', 'علی');         -- drop the decision
SELECT * FROM graphwright.decisions('notes');              -- the audit log
```

Every merge is reversible, including an exact fold of two identical spellings (two people both written `Sara`). Find the occurrence and pin it to its own entity:

```sql
SELECT entity_id, source_pk, surface_form FROM graphwright.mentions('notes');
SELECT graphwright.split_mention('notes', '(0,2)', 'Sara');    -- separate one occurrence
SELECT graphwright.unsplit_mention('notes', '(0,2)', 'Sara');  -- fold it back
```

Each applies immediately and is reversible: edit or delete the underlying row and the graph re-derives without it. The rest of the cascade (embedding, full NFKC) is still to come.

## Build

```bash
cargo pgrx run pg17      # build, install, open a psql
cargo pgrx test pg17     # run the regression tests
```

Requires the pgrx toolchain (`cargo install cargo-pgrx`, then `cargo pgrx init`). Built against `pgrx 0.18`.

## License

AGPL-3.0-only. See [LICENSE](LICENSE).
