-- Typed relationships: directed, labeled edges from a relation extractor.
--
--   psql -f examples/typed-edges.sql
--
-- By default an edge means "these two names appeared together" (undirected
-- co-mention). Point graphwright.relation_extractor at a SQL function that
-- returns (subject, predicate, object) triples and edges become directed and
-- typed: who did what to whom. Same extension-point shape as the entity
-- extractor, so the model proposes and a human can still merge or split.
--
-- Test data uses invented names only.

CREATE EXTENSION IF NOT EXISTS pg_graphwright;

DROP TABLE IF EXISTS memos CASCADE;
CREATE TABLE memos (id int PRIMARY KEY, body text);
INSERT INTO memos VALUES
  (1, 'Dana manages Priya.'),
  (2, 'Ravi mentors Kimi.'),
  (3, 'Joe closed Globex.'),
  (4, 'Nadia signed Globex.');

-- Entity extractor: capitalized words are people and organizations. Sentences
-- start with a name here, so no stop-list is needed.
CREATE OR REPLACE FUNCTION people(doc text) RETURNS text[] LANGUAGE sql IMMUTABLE AS $$
  SELECT array_agg(w) FROM regexp_split_to_table(doc, '[^[:alpha:]]+') AS w
  WHERE w ~ '^[[:upper:]]'
$$;
SET graphwright.extractor = 'people';

CREATE INDEX memos_kg ON memos USING graphwright (body);
SELECT graphwright.maintain();

-- Without a relation extractor: undirected co-mention. Every pair of names in
-- a memo is one 'co_mentioned' edge, with no sense of who did what.
SELECT src, predicate, dst FROM graphwright.edges('memos') ORDER BY src, dst;
--  Dana   | co_mentioned | Priya
--  Globex | co_mentioned | Nadia
--  Joe    | co_mentioned | Globex
--  Ravi   | co_mentioned | Kimi

-- The relation extractor: pull (subject, predicate, object) from each memo as
-- a flat text[]. A real deployment points this at an LLM or a relation model;
-- here a few verbs are enough to show the shape.
CREATE OR REPLACE FUNCTION relations(doc text) RETURNS text[] LANGUAGE sql IMMUTABLE AS $$
  SELECT array_agg(part ORDER BY ord, idx)
  FROM (
    SELECT row_number() OVER () AS ord, m
    FROM regexp_matches(
      doc, '([[:upper:]][[:alpha:]]+) (manages|mentors|closed|signed) ([[:upper:]][[:alpha:]]+)', 'g'
    ) AS m
  ) matches,
  LATERAL unnest(ARRAY[m[1], m[2], m[3]]) WITH ORDINALITY AS u(part, idx)
$$;
SET graphwright.relation_extractor = 'relations';

-- No reindex needed: maintain() re-resolves from the same stored extraction.
SELECT graphwright.maintain();

-- Now edges are directed and typed: subject, relation, object.
SELECT src, predicate, dst FROM graphwright.edges('memos') ORDER BY src, dst;
--  Dana  | manages | Priya
--  Joe   | closed  | Globex
--  Nadia | signed  | Globex
--  Ravi  | mentors | Kimi

-- Visibility, identity resolution, and the reversible decision log are
-- unchanged: typed edges still follow the source rows' row-level security,
-- and graphwright.split/merge still apply to their endpoints.
