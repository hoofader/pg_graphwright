-- Upgrade the diary from the toy extractor to real NER, with no schema change.
--
-- The extractor is an extension point (graphwright.extractor = a SQL function f(text)->
-- text[]). This swaps the capitals-regex stand-in for a real GLiNER model,
-- which finds entities the heuristic misses: lowercase and multi-word names
-- and places ("old bazaar"), and skips non-entities by meaning, not case.
--
-- Prerequisites:
--   1. Run examples/diary/schema.sql (and demo.sql for some entries).
--   2. Run the graphwright-onnx model service on :8787 (see its README):
--        GRAPHWRIGHT_ONNX_MODEL_ID=onnx-community/gliner_small-v2.1 pnpm serve
--   3. Install pgsql-http (the `http` extension) for a request from SQL.
--
--   psql -f examples/diary/onnx.sql

CREATE EXTENSION IF NOT EXISTS http;

-- The new extractor: POST each entry to the model service, return its
-- surfaces. Identical in shape to ../gliner-extractor.sql.
CREATE OR REPLACE FUNCTION gliner_extract(doc text) RETURNS text[]
    LANGUAGE sql VOLATILE AS $$
    WITH resp AS (
        SELECT content FROM http_post(
            'http://localhost:8787/extract',
            jsonb_build_object('text', doc)::text,
            'application/json')
    )
    SELECT COALESCE(
        (SELECT array_agg(s)
         FROM resp, jsonb_array_elements_text((content::jsonb) -> 'surfaces') AS s),
        ARRAY[]::text[]);
$$;

-- Point the diary at it, for every session on this database.
DO $$ BEGIN
    EXECUTE format('ALTER DATABASE %I SET graphwright.extractor = %L',
                   current_database(), 'gliner_extract');
END $$;
SET graphwright.extractor = 'gliner_extract';  -- and this session

-- Re-extract the existing entries through GLiNER. REINDEX re-marks every row;
-- maintain() then runs them through the new extractor. (New writes need no
-- REINDEX -- aminsert marks them and the next maintain() uses the extension point.)
REINDEX INDEX diary_kg;
SELECT graphwright.maintain();

-- The graph now holds what a model finds -- including lowercase and
-- multi-word entities the capitals heuristic dropped.
SELECT surface FROM graphwright.entities('diary') ORDER BY surface;

-- To go back to the toy extractor:
--   ALTER DATABASE :db SET graphwright.extractor = 'diary_names';
--   REINDEX INDEX diary_kg; SELECT graphwright.maintain();
