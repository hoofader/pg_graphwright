-- A personal diary, with pg_graphwright as the knowledge-graph backend.
--
-- This is the reusable schema: the table, its row-level security, the
-- extraction seam, the graph index, and an app-facing API of per-diarist
-- views. Run it once, then examples/diary/demo.sql for the walkthrough.
--
--   psql -f examples/diary/schema.sql
--
-- The idea: a diary is private, and the graph of who you write about must be
-- exactly as private. pg_graphwright derives that graph as a Postgres index
-- and lets the SAME row-level security govern it. There is no second access
-- model to keep in sync, and no way to read the graph around it.

CREATE EXTENSION IF NOT EXISTS pg_graphwright;

DROP VIEW IF EXISTS my_people, my_circle, my_review_queue;
DROP TABLE IF EXISTS diary CASCADE;

-- One row per entry. The diarist owns it; row-level security is the only
-- privacy boundary, and the knowledge graph inherits it.
CREATE TABLE diary (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    diarist    text NOT NULL DEFAULT current_user,
    written_at timestamptz NOT NULL DEFAULT now(),
    entry      text NOT NULL
);
ALTER TABLE diary ENABLE ROW LEVEL SECURITY;
CREATE POLICY own_diary ON diary
    USING (diarist = current_user) WITH CHECK (diarist = current_user);
GRANT SELECT, INSERT, UPDATE, DELETE ON diary TO PUBLIC;

-- What counts as an entity in a diary is the people (and places) it names.
-- This toy extractor keeps capitalized Latin words and any non-Latin word
-- (so Persian/Cyrillic names survive), dropping lowercase function words. A
-- real app points graphwright.extractor at GLiNER or an LLM instead -- see
-- ../gliner-extractor.sql; the seam is just a SQL function f(text)->text[].
CREATE OR REPLACE FUNCTION diary_names(doc text) RETURNS text[] LANGUAGE sql IMMUTABLE AS $$
    SELECT array_agg(w)
    FROM regexp_split_to_table(doc, '[^[:alnum:]]+') AS w
    WHERE w <> '' AND w !~ '^[a-z]+$'
      AND lower(w) NOT IN ('i', 'today', 'tonight', 'then', 'this', 'last')
$$;

-- Configure the extractor for every session on this database (including the
-- background maintenance worker), not just this one.
DO $$ BEGIN
    EXECUTE format('ALTER DATABASE %I SET graphwright.extractor = %L',
                   current_database(), 'diary_names');
END $$;

-- The knowledge-graph index over the entry text. Extraction and resolution
-- run off the write path; the graph catches up on graphwright.maintain() or,
-- in production, the background worker (set graphwright.database and add the
-- extension to shared_preload_libraries).
CREATE INDEX diary_kg ON diary USING graphwright (entry);

-- The app-facing API. security_invoker makes the accessor run as the CALLER,
-- so each diarist's `SELECT * FROM my_people` returns only their own graph --
-- no WHERE clause, no tenant column, no second access-control system.
CREATE VIEW my_people WITH (security_invoker = true) AS
    SELECT surface AS person FROM graphwright.entities('diary');

CREATE VIEW my_circle WITH (security_invoker = true) AS
    SELECT src AS person, dst AS also FROM graphwright.edges('diary');

CREATE VIEW my_review_queue WITH (security_invoker = true) AS
    SELECT surface_a AS maybe_same_as, surface_b FROM graphwright.proposals('diary');

GRANT SELECT ON my_people, my_circle, my_review_queue TO PUBLIC;
