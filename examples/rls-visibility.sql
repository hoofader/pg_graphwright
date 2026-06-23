-- pg_graphwright example: the graph's visibility follows the source rows.
--
-- The position no other knowledge-graph tool takes: an entity or edge is
-- visible to you exactly when the source row it came from is. There is no
-- second access-control system to keep in sync with your data; the extension
-- delegates to Postgres row-level security. A DIRECT read of the catalog is
-- filtered the same way the accessors are, so the accessors are no back door.
--
--   psql -f examples/rls-visibility.sql      (against an installed extension)

CREATE EXTENSION IF NOT EXISTS pg_graphwright;

-- Idempotent reset so the file re-runs cleanly.
DROP TABLE IF EXISTS journal CASCADE;
DROP ROLE IF EXISTS mina;
DROP ROLE IF EXISTS arman;

-- A journaling app: each person owns their own entries.
CREATE TABLE journal (id int PRIMARY KEY, author text, entry text);
ALTER TABLE journal ENABLE ROW LEVEL SECURITY;
CREATE POLICY own_entries ON journal USING (author = current_user);
GRANT SELECT ON journal TO PUBLIC;

CREATE ROLE mina;
CREATE ROLE arman;

-- A toy extractor: capitalized words that are not common sentence-starters.
-- Real deployments point graphwright.extractor at GLiNER or an LLM (see
-- examples/gliner-extractor.sql); the extension point is just a SQL function f(text)->text[].
CREATE OR REPLACE FUNCTION names_only(doc text) RETURNS text[] LANGUAGE sql IMMUTABLE AS $$
  SELECT array_agg(w)
  FROM regexp_split_to_table(doc, '[^[:alpha:]]+') AS w
  WHERE w ~ '^[[:upper:]]' AND lower(w) NOT IN ('had', 'the', 'and', 'met', 'then', 'ran', 'into')
$$;
SET graphwright.extractor = 'names_only';

INSERT INTO journal VALUES
  (1, 'mina',  'coffee with Sara and Kaveh'),
  (2, 'mina',  'Sara lent me a book by Borges'),
  (3, 'arman', 'ran into Sara and Darya at the gallery');

-- CREATE INDEX only marks the rows; the resolved graph builds on the next
-- maintain(), which runs as the owner over every row (extraction is off the
-- write path, so a slow model never blocks a write).
CREATE INDEX journal_kg ON journal USING graphwright (entry);
SELECT graphwright.maintain();

-- As the owner, the whole graph: sara, kaveh, borges, darya.
SELECT surface FROM graphwright.entities('journal') ORDER BY surface;

-- mina sees only what HER entries support: sara, kaveh, borges (not darya),
-- and the co-mention edges among them.
SET ROLE mina;
SELECT surface FROM graphwright.entities('journal') ORDER BY surface;
SELECT src, dst FROM graphwright.edges('journal') ORDER BY src, dst;
RESET ROLE;

-- arman sees only his: sara, darya (not kaveh, not borges). And the punch:
-- a DIRECT catalog read is filtered identically, so the accessors are not a
-- privileged door around row security.
SET ROLE arman;
SELECT surface FROM graphwright.entities('journal') ORDER BY surface;
SELECT count(*) AS entities_arman_can_see FROM graphwright.entity;  -- < the full graph
RESET ROLE;
