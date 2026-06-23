# Diary: an end-to-end pg_graphwright application

A small but complete worked example: a private journaling app whose backend
is a `pg_graphwright` index. It is the use case the extension was built for:
a diary is private, and the graph of who you write about must be exactly as
private, with no second access-control system to keep in sync.

```bash
psql -f examples/diary/schema.sql   # the reusable schema (run once)
psql -f examples/diary/demo.sql     # a two-diarist walkthrough
```

Run `demo.sql` in a fresh session after `schema.sql`, so the database-level
extractor setting is in effect.

## What it shows

- **The graph is as private as the diary.** Each entry is owned by its
  diarist under row-level security. The entities and edges derived from it
  inherit that policy: `Emma` sees her own people and connections, `Jack`
  sees only his. The same is true of a direct `SELECT * FROM graphwright.entity`,
  so the views are no privileged back door.
- **An app API that is just views.** `my_people`, `my_circle`, and
  `my_review_queue` are `security_invoker` views over the accessors, so a
  diarist's `SELECT * FROM my_people` returns their own graph with no `WHERE`
  clause and no tenant column. Row security does the filtering.
- **Resolution with a human in the loop.** `Sara` and `Sarah` (the same short
  name spelled two ways) are too ambiguous to auto-merge, so they surface in
  `my_review_queue`; the diarist confirms and the app applies
  `graphwright.merge` (reversible).
- **It tracks edits like an index.** Editing an entry updates the graph on
  the next `graphwright.maintain()` (or background-worker) tick.

## The files

- `schema.sql`: the diary table + its policy, the extraction extension point
  (`graphwright.extractor` = a toy `diary_names`), the `USING graphwright`
  index, and the `security_invoker` app views.
- `demo.sql`: creates two diarists, writes their entries, builds the graph,
  and walks privacy, the review/merge handoff, a live edit, and a
  most-connected-people query.
- `onnx.sql`: swaps the toy extractor for a real GLiNER model, with no
  schema change.

## Real NER (`onnx.sql`)

The capitals extractor is a stand-in. Because extraction is an extension point, you swap
in a real model by pointing `graphwright.extractor` at a different SQL
function, with no schema change. `onnx.sql` points it at the
[graphwright-onnx](https://github.com/hoofader/graphwright-onnx) GLiNER
service (over `pgsql-http`) and re-extracts.

The difference is real. On *"met Tom near the old town library"*:

| extractor | entities found |
|-----------|----------------|
| toy capitals regex | `Tom` |
| GLiNER (ONNX) | `Tom`, `old town library` |

The model recovers a lowercase, multi-word *place* the heuristic can't see,
and skips content words (`coffee`, `lunch`) by meaning rather than case. The
model runs in its own process, not the database backend; Postgres only POSTs
the text and gets back surfaces. Verified end to end.

It is not flawless: the model also tags the pronoun `I` as a person.
`graphwright.judge` (a second SQL-function extension point) is where you trim that kind
of model noise before it reaches the graph.

## Honest scope

Edges in this demo are co-mention (two names written near each other). For
directed, typed relationships, point `graphwright.relation_extractor` at a
relation function (see `../typed-edges.sql`). Identity is resolved globally by
name, while visibility is per diarist; to keep two people who share a name
apart, `graphwright.split_mention` separates one occurrence onto its own
identity (see `../identity-resolution.sql`). The toy capitals extractor is a
stand-in for real NER through the extension point.
