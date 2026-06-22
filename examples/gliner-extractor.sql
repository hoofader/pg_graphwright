-- Wire pg_graphwright's extractor seam to a graphwright-onnx model service.
--
-- The extension keeps the model runtime OUT of the backend: extraction is a
-- SQL function f(text) -> text[]. Here that function calls a small HTTP
-- service (graphwright-onnx) that runs GLiNER. Extraction runs off the write
-- path (the maintenance worker / graphwright.maintain() drives it), so a
-- blocking HTTP call here never slows a write.
--
-- 1. Start the service (see the graphwright-onnx README):
--      GRAPHWRIGHT_ONNX_MODEL_ID=onnx-community/gliner_small-v2.1 pnpm serve
-- 2. Install pgsql-http for a synchronous request from SQL.
-- 3. Run this file, then CREATE INDEX ... USING graphwright (body): the graph
--    fills with GLiNER-extracted entities on the next maintain()/worker tick.

CREATE EXTENSION IF NOT EXISTS http;

-- Edit the URL for your deployment. Returns the surfaces array, or an empty
-- array if the service answers without one.
CREATE OR REPLACE FUNCTION gliner_extract(doc text) RETURNS text[]
    LANGUAGE sql VOLATILE AS $$
    WITH resp AS (
        SELECT content
        FROM http_post(
            'http://localhost:8787/extract',
            jsonb_build_object('text', doc)::text,
            'application/json'
        )
    )
    SELECT COALESCE(
        (SELECT array_agg(s)
         FROM resp, jsonb_array_elements_text((content::jsonb) -> 'surfaces') AS s),
        ARRAY[]::text[]
    );
$$;

-- Point the seam at it. (A judge can vet the result too: graphwright.judge.)
SET graphwright.extractor = 'gliner_extract';

-- No pgsql-http? The same call works from plpython3u with urllib, or wire any
-- other gateway. The only contract is f(text) -> text[].
