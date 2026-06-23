-- Enterprise walkthrough. Run examples/enterprise/schema.sql first (in its
-- own session, so the database-level extractor setting is in effect here).
--
--   psql -f examples/enterprise/schema.sql
--   psql -f examples/enterprise/demo.sql
--
-- Five employees across three teams. Each sees the knowledge graph built from
-- exactly the documents their access rules allow, enforced by row-level
-- security. Then a sales doc is shared with an engineer, joins her graph, and
-- is unshared again, all with no graph rebuild.

-- Run this as the extension owner (the role that created it): it inserts on
-- everyone's behalf, calls the owner-only maintenance functions, and SET
-- ROLEs into the employees.

-- Employees. kimi and ravi are engineers (members of the engineering role);
-- joe and mia are in sales; dana is in marketing.
DROP ROLE IF EXISTS kimi, ravi, joe, mia, dana;
CREATE ROLE kimi LOGIN; CREATE ROLE ravi LOGIN;
CREATE ROLE joe LOGIN;  CREATE ROLE mia LOGIN;
CREATE ROLE dana LOGIN;
GRANT engineering TO kimi, ravi;

DELETE FROM docs;  -- a clean slate if the demo is re-run

-- The documents. Inserted by the owner with each doc's team and author, the
-- way an app would write them on the user's behalf.
INSERT INTO docs (dept, owner, title, body) VALUES
  ('engineering', 'kimi', 'Sprint notes',
   'Kimi paired with Ravi on the Stark integration all week.'),
  ('engineering', 'ravi', 'Pipeline debugging',
   'Ravi met Tessa from Stark to debug the data pipeline.'),
  ('marketing', 'dana', 'Campaign recap',
   'Dana shipped the Initech campaign with help from Priya.'),
  ('sales', 'joe', 'Globex account',
   'Joe finally closed the Globex account. Nadia at Globex signed the contract.'),
  ('sales', 'mia', 'Umbrella pursuit',
   'Mia is courting Umbrella. Victor at Umbrella is the champion.');

-- Build the graph. maintain() runs as the owner over every document, so the
-- whole graph is derived once; who sees what is decided later, per caller.
SELECT graphwright.maintain();

-- Kimi (engineering) sees both engineering docs and the marketing doc, and
-- nothing from sales. Sales is private to its owner.
SET ROLE kimi;
SELECT who FROM my_people ORDER BY who;   -- Dana, Initech, Kimi, Priya, Ravi, Stark, Tessa
RESET ROLE;

-- Joe (sales) sees his own sales doc and the marketing doc. He does not see
-- engineering docs, and not Mia's sales doc.
SET ROLE joe;
SELECT who FROM my_people ORDER BY who;   -- Dana, Globex, Initech, Joe, Nadia, Priya
RESET ROLE;

-- Mia (sales) sees her own sales doc and marketing. Joe's Globex deal is not
-- in her graph: salespeople do not see each other's accounts.
SET ROLE mia;
SELECT who FROM my_people ORDER BY who;   -- Dana, Initech, Mia, Priya, Umbrella, Victor
RESET ROLE;

-- Dana (marketing) is not an engineer and owns no sales docs, so her graph is
-- the marketing doc alone.
SET ROLE dana;
SELECT who FROM my_people ORDER BY who;   -- Dana, Initech, Priya
RESET ROLE;

-- ---------------------------------------------------------------------------
-- Sharing. Joe shares the Globex account doc with Kimi.
--
-- Nothing about the graph is rebuilt. The Globex entities were always in the
-- catalog (maintain() read every doc). The share only changes what Kimi's
-- row-level security lets her read, and the graph follows that immediately.
-- ---------------------------------------------------------------------------
SET ROLE joe;
INSERT INTO doc_shares (doc_id, shared_with)
    SELECT id, 'kimi' FROM docs WHERE dept = 'sales' AND owner = 'joe';
RESET ROLE;

-- Kimi's graph now includes Globex, Nadia, and Joe. No graphwright.maintain()
-- ran between the two queries.
SET ROLE kimi;
SELECT who FROM my_people ORDER BY who;   -- ...now with Globex, Joe, Nadia
RESET ROLE;

-- Joe unshares it.
SET ROLE joe;
DELETE FROM doc_shares WHERE shared_with = 'kimi';
RESET ROLE;

-- And the Globex doc leaves Kimi's graph, again with no rebuild.
SET ROLE kimi;
SELECT who FROM my_people ORDER BY who;   -- back to engineering + marketing only
RESET ROLE;

-- A direct catalog read is filtered the same way, so the views are no
-- privileged back door: Dana sees only her marketing entities here too.
SET ROLE dana;
SELECT count(*) AS entities_dana_can_see FROM graphwright.entity;
RESET ROLE;

-- Note on scope: edges are co-mention only (two names in the same doc), not
-- typed relationships, and identity is resolved globally by name while
-- visibility is per employee. The toy capitals extractor is a stand-in for
-- real NER through the extension point (see ../gliner-extractor.sql).
