# Diary: an end-to-end pg_graphwright application

A small but complete worked example — a private journaling app whose backend
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
  inherit that policy: `leila` sees her own people and connections, `omid`
  sees only his. The same is true of a direct `SELECT * FROM graphwright.entity`
  — the views are no privileged back door.
- **An app API that is just views.** `my_people`, `my_circle`, and
  `my_review_queue` are `security_invoker` views over the accessors, so a
  diarist's `SELECT * FROM my_people` returns their own graph with no `WHERE`
  clause and no tenant column. Row security does the filtering.
- **Resolution with a human in the loop.** `Sara` and the Persian `سارا` are
  too short a name to auto-merge, so they surface in `my_review_queue`; the
  diarist confirms and the app applies `graphwright.merge` (reversible).
- **It tracks edits like an index.** Editing an entry updates the graph on
  the next `graphwright.maintain()` (or background-worker) tick.

## The files

- `schema.sql` — the diary table + its policy, the extraction seam
  (`graphwright.extractor` = a toy `diary_names`; swap in GLiNER via
  `../gliner-extractor.sql`), the `USING graphwright` index, and the
  `security_invoker` app views.
- `demo.sql` — creates two diarists, writes their entries, builds the graph,
  and walks privacy, the review/merge handoff, a live edit, and a
  most-connected-people query.

## Honest scope

Edges are co-mention only (two names written near each other), not typed
relationships. Identity is resolved globally by name, while visibility is per
diarist; to keep two people who share a name apart, `graphwright.split_mention`
separates one occurrence onto its own identity (see `../identity-resolution.sql`).
The toy capitals extractor is a stand-in for real NER through the seam.
