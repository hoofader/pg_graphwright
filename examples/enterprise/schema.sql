-- An enterprise document store, with pg_graphwright as the knowledge-graph
-- backend. Engineering, sales, and marketing each have their own access
-- rules, and every employee's view of the graph is exactly the documents
-- they can read.
--
--   psql -f examples/enterprise/schema.sql   # the reusable schema (run once)
--   psql -f examples/enterprise/demo.sql      # the walkthrough
--
-- The access rules (the point of the example):
--   * engineering docs  -> any member of the `engineering` role,
--   * marketing docs     -> everyone,
--   * sales docs          -> the owner only,
--   * plus any document explicitly shared with you (the doc_shares table).
--
-- The graph derived from the docs inherits all of this. There is no second
-- access model: the same row-level security that decides who can read a
-- document decides whose knowledge graph it appears in. Share a doc and it
-- joins the recipient's graph; unshare it and it leaves, with no rebuild.

CREATE EXTENSION IF NOT EXISTS pg_graphwright;

-- Engineering is a Postgres group role: membership is the access check for
-- engineering docs. Sales and marketing access is ownership- and
-- public-based, so they need no role here. Idempotent so the file re-runs.
DO $$ BEGIN
    CREATE ROLE engineering;
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

DROP VIEW IF EXISTS my_people, my_circle, my_review_queue;
DROP TABLE IF EXISTS doc_shares, docs CASCADE;

-- One row per document. `dept` and `owner` drive the access rules; `body` is
-- the text the graph is built from.
CREATE TABLE docs (
    id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    dept    text NOT NULL CHECK (dept IN ('engineering', 'sales', 'marketing')),
    owner   text NOT NULL DEFAULT current_user,
    title   text NOT NULL,
    body    text NOT NULL
);

-- Explicit, revocable sharing: one row grants one user access to one doc.
CREATE TABLE doc_shares (
    doc_id      bigint NOT NULL REFERENCES docs(id) ON DELETE CASCADE,
    shared_with text   NOT NULL,
    PRIMARY KEY (doc_id, shared_with)
);

-- The docs policy checks doc_shares, and the doc_shares policy checks docs.
-- That mutual reference recurses, so the docs policy reads the share table
-- through this SECURITY DEFINER function, which bypasses doc_shares' own row
-- security and breaks the cycle. `who` is passed in (current_user inside a
-- definer function is the function owner, not the caller). This is the same
-- trick the extension uses for its edge-support policy.
CREATE FUNCTION shared_with_me(d bigint, who text) RETURNS boolean
    LANGUAGE sql STABLE SECURITY DEFINER AS $$
    SELECT EXISTS (SELECT 1 FROM doc_shares WHERE doc_id = d AND shared_with = who)
$$;

-- The access rule, as one row-level-security policy. A document is visible
-- when any clause holds. The graph reads this same policy through the
-- accessors, so a user's graph is exactly the docs this returns.
ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
CREATE POLICY doc_access ON docs USING (
    dept = 'marketing'
    OR (dept = 'engineering' AND pg_has_role(current_user, 'engineering', 'MEMBER'))
    OR (dept = 'sales' AND owner = current_user)
    OR shared_with_me(id, current_user)
);
GRANT SELECT ON docs TO PUBLIC;

-- You can read a share that targets you or that involves a doc you own; you
-- can only create or drop a share for a document you own.
ALTER TABLE doc_shares ENABLE ROW LEVEL SECURITY;
CREATE POLICY share_select ON doc_shares FOR SELECT USING (
    shared_with = current_user
    OR EXISTS (SELECT 1 FROM docs d WHERE d.id = doc_id AND d.owner = current_user));
CREATE POLICY share_modify ON doc_shares FOR ALL
    USING (EXISTS (SELECT 1 FROM docs d WHERE d.id = doc_id AND d.owner = current_user))
    WITH CHECK (EXISTS (SELECT 1 FROM docs d WHERE d.id = doc_id AND d.owner = current_user));
GRANT SELECT, INSERT, DELETE ON doc_shares TO PUBLIC;

-- What counts as an entity is the people and organizations a doc names. This
-- toy extractor keeps capitalized words and stop-lists the common ones, the
-- same stand-in the diary example uses. A real deployment points
-- graphwright.extractor at GLiNER or an LLM instead (see ../gliner-extractor.sql);
-- the extension point is just a SQL function f(text) -> text[].
CREATE OR REPLACE FUNCTION doc_names(doc text) RETURNS text[] LANGUAGE sql IMMUTABLE AS $$
    SELECT array_agg(w)
    FROM regexp_split_to_table(doc, '[^[:alpha:]]+') AS w
    WHERE w ~ '^[[:upper:]]'
      AND lower(w) NOT IN (
        'i','a','an','the','this','that','these','those','here','there',
        'my','our','your','his','her','its','their','we','you','he','she','it','they',
        'and','but','or','so','if','then','as','at','by','for','from','in','of','on','to','with',
        'about','after','before','while','when','where','why','how',
        'am','is','are','was','were','be','been','have','has','had','do','did','will','would',
        'could','should','can','may','might','just','not','no','also','still','even','only',
        'very','really','maybe','another','every','some','any','more','most','all','both',
        'today','tonight','tomorrow','yesterday','now','later','soon','always','never','often',
        'busy','quiet','long','good','great','nice','tired','happy','new','old','last','next','early',
        'monday','tuesday','wednesday','thursday','friday','saturday','sunday',
        'january','february','march','april','june','july','august','september','october','november','december',
        'morning','afternoon','evening','night','week','weekend','day','work','office','home','lunch','dinner','coffee')
$$;

-- Configure the extractor for every session on this database (including the
-- background maintenance worker), not just this one.
DO $$ BEGIN
    EXECUTE format('ALTER DATABASE %I SET graphwright.extractor = %L',
                   current_database(), 'doc_names');
END $$;

-- The knowledge-graph index over the document text.
CREATE INDEX docs_kg ON docs USING graphwright (body);

-- The app-facing API. security_invoker runs each accessor as the CALLER, so
-- an employee's `SELECT * FROM my_people` returns only the graph from the
-- docs they can read. No WHERE clause, no tenant column, no second ACL.
CREATE VIEW my_people WITH (security_invoker = true) AS
    SELECT surface AS who FROM graphwright.entities('docs');

CREATE VIEW my_circle WITH (security_invoker = true) AS
    SELECT src AS who, dst AS also FROM graphwright.edges('docs');

CREATE VIEW my_review_queue WITH (security_invoker = true) AS
    SELECT surface_a AS maybe_same_as, surface_b FROM graphwright.proposals('docs');

GRANT SELECT ON my_people, my_circle, my_review_queue TO PUBLIC;
