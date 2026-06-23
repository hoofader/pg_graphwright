# Examples

Each file is runnable in `psql` against an installed extension:

```bash
psql -f examples/<name>.sql
```

They build the graph with a toy capitals-regex extractor so the demos stay
self-contained; swap in real NER through the seam (see `gliner-extractor.sql`).
Test data uses invented names only.

| File | What it shows | Why it matters |
|------|---------------|----------------|
| [`rls-visibility.sql`](rls-visibility.sql) | An entity/edge is visible exactly when the source row it came from is. A direct catalog read is filtered the same as the accessors. | The position no other knowledge-graph tool takes: graph access delegates to Postgres row-level security, with no second ACL and no back door. |
| [`edge-disclosure.sql`](edge-disclosure.sql) | A relationship supported by several rows is disclosed under a per-watch rule: `union` (read any supporting row) or `intersection` (read every one). | Need-to-know on relationships, not just rows. Nothing else models an edge backed by N rows with N different ACLs. |
| [`identity-resolution.sql`](identity-resolution.sql) | Cross-script names auto-merge (`Khashayar`/`خشایار`, `Khabarov`/`Хабаров`); short ambiguous ones become review proposals; every merge/split is durable and reversible, down to splitting two identical spellings apart. | Deterministic, multilingual resolution with a human who can overrule it after the fact. Apply-then-review, not a baked-in pipeline side effect. |
| [`gliner-extractor.sql`](gliner-extractor.sql) | Point `graphwright.extractor` at a GLiNER model service (via `pgsql-http`). | The extractor is a model-agnostic SQL-function seam; no model runtime in the backend. |

Two things every example does, because they trip people up:

- `SELECT graphwright.maintain();` after `CREATE INDEX` and after each write —
  extraction and resolution run off the write path, so the graph is empty
  until a maintenance tick (or the background worker) runs.
- The maintenance and review functions (`maintain`, `merge`, `split`, …) run
  as the extension owner and are revoked from `PUBLIC`. Only the read
  accessors (`entities`, `edges`, `mentions`, `proposals`, `decisions`) are
  called under `SET ROLE` to demonstrate row-level-security filtering.
