-- pg_graphwright example: how much a relationship discloses is a policy.
--
-- An edge can be supported by several source rows, owned by different people.
-- Each watch chooses a disclosure rule:
--   union         the edge is visible if you can read ANY supporting row
--   intersection  the edge is visible only if you can read EVERY supporting row
-- No other knowledge-graph tool even has the concept of a relationship
-- supported by N rows with N different access rules.
--
--   psql -f examples/edge-disclosure.sql     (against an installed extension)

CREATE EXTENSION IF NOT EXISTS pg_graphwright;

DROP TABLE IF EXISTS memos CASCADE;
DROP ROLE IF EXISTS deal_alpha;
DROP ROLE IF EXISTS deal_beta;

-- A shared deal room: each banking team owns its own memos.
CREATE TABLE memos (id int PRIMARY KEY, team text, body text);
ALTER TABLE memos ENABLE ROW LEVEL SECURITY;
CREATE POLICY by_team ON memos USING (team = current_user);
GRANT SELECT ON memos TO PUBLIC;

CREATE ROLE deal_alpha;
CREATE ROLE deal_beta;

CREATE OR REPLACE FUNCTION names_only(doc text) RETURNS text[] LANGUAGE sql IMMUTABLE AS $$
  SELECT array_agg(w)
  FROM regexp_split_to_table(doc, '[^[:alpha:]]+') AS w
  WHERE w ~ '^[[:upper:]]' AND lower(w) NOT IN ('call', 'with', 'and')
$$;
SET graphwright.extractor = 'names_only';

-- The same pairing (Aria - Kambiz) is named in two memos owned by two teams,
-- so the edge between them is supported by both rows.
INSERT INTO memos VALUES
  (1, 'deal_alpha', 'call with Aria and Kambiz'),
  (2, 'deal_beta',  'Aria countersigned with Kambiz');
CREATE INDEX memos_kg ON memos USING graphwright (body);
SELECT graphwright.maintain();

-- union (the default): a deal_alpha banker sees the edge via their own memo,
-- even though they cannot read deal_beta's.
SET ROLE deal_alpha;
SELECT src, dst FROM graphwright.edges('memos');   -- aria <-> kambiz
RESET ROLE;

-- Tighten the disclosure rule to intersection. (Visibility is read live, so
-- no re-resolve is needed.)
UPDATE graphwright.watch SET visibility = 'intersection'
  WHERE source_table = 'memos'::regclass;

-- Now the edge hides from anyone who cannot read EVERY memo behind it. The
-- deal_alpha banker still sees the two parties exist (entities are union),
-- but the link between them requires seeing both supporting memos.
SET ROLE deal_alpha;
SELECT surface FROM graphwright.entities('memos') ORDER BY surface;  -- aria, kambiz
SELECT src, dst FROM graphwright.edges('memos');                     -- (no rows)
RESET ROLE;

-- The owner, who reads every memo, still sees the edge under intersection.
SELECT src, dst FROM graphwright.edges('memos');                     -- aria <-> kambiz
