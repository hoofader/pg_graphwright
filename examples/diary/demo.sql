-- Diary walkthrough. Run examples/diary/schema.sql first (in its own
-- session, so the database-level extractor setting is in effect here).
--
--   psql -f examples/diary/schema.sql
--   psql -f examples/diary/demo.sql
--
-- Two diarists, leila and omid. Each writes only their own entries and sees
-- only their own knowledge graph -- enforced by row-level security, the same
-- boundary that protects the diary text itself.

-- Run this as the extension owner (the role that created it): it calls the
-- owner-only maintenance/review functions and SET ROLEs into the diarists.
DROP ROLE IF EXISTS leila;
DROP ROLE IF EXISTS omid;
CREATE ROLE leila;
CREATE ROLE omid;

DELETE FROM diary;  -- a clean slate if the demo is re-run

-- leila's diary. Her friend Sara also shows up in Persian as سارا; she sees
-- Kaveh often and Darya once. Entries are inserted AS leila, so the diarist
-- column and the row-security policy line up.
SET ROLE leila;
INSERT INTO diary (entry) VALUES
    ('coffee with Sara and Kaveh this morning'),
    ('سارا called, made plans with Darya'),
    ('long walk with Kaveh');
RESET ROLE;

-- omid keeps his own diary, a separate world.
SET ROLE omid;
INSERT INTO diary (entry) VALUES
    ('Sara from the office stopped by'),
    ('dinner with Babak');
RESET ROLE;

-- Build the graph. In production the background worker does this on a tick;
-- here we run it once. maintain() runs as the owner over every row.
SELECT graphwright.maintain();

-- leila opens her app. She queries plain views; row security makes the
-- results hers alone. Sara and سارا are still two nodes (too short a name to
-- auto-merge), and the app surfaces them as a review suggestion.
SET ROLE leila;
SELECT person FROM my_people ORDER BY person;              -- Darya, Kaveh, Sara, سارا
SELECT person, also FROM my_circle ORDER BY person, also;  -- her co-mentions
SELECT * FROM my_review_queue;                             -- Sara <-> سارا ?
RESET ROLE;

-- omid sees ONLY his diary's graph. leila's Darya/Kaveh/سارا do not exist for
-- him, and a DIRECT catalog read is filtered the same way -- the views are no
-- privileged back door.
SET ROLE omid;
SELECT person FROM my_people ORDER BY person;              -- Babak, Sara
SELECT count(*) AS entities_omid_can_see FROM graphwright.entity;
RESET ROLE;

-- leila confirms the suggestion. The app applies it on her behalf (a
-- privileged action: the review functions run as the owner). The decision is
-- durable and replayed on every re-resolve.
SELECT graphwright.merge('diary', 'Sara', 'سارا');
SELECT graphwright.maintain();
SET ROLE leila;
SELECT person FROM my_people ORDER BY person;              -- Sara and سارا now one
RESET ROLE;
-- ...and it is reversible -- graphwright.unmerge('diary','Sara','سارا').

-- Live: leila edits an entry, and the graph tracks the change on the next
-- maintenance tick, like any index.
SET ROLE leila;
UPDATE diary SET entry = 'long walk with Kaveh and Nima'
    WHERE entry = 'long walk with Kaveh';
RESET ROLE;
SELECT graphwright.maintain();
SET ROLE leila;
SELECT person FROM my_people ORDER BY person;              -- Nima has joined
RESET ROLE;

-- A query a diary app would actually run: leila's most-connected people.
SET ROLE leila;
SELECT person, count(*) AS times_together
FROM (SELECT person FROM my_circle UNION ALL SELECT also FROM my_circle) t
GROUP BY person
ORDER BY times_together DESC, person;
RESET ROLE;

-- Note on scope: identity is resolved globally by name (leila's Sara and
-- omid's Sara are the same NODE), but visibility is per diarist. To keep two
-- people who share a name apart, split one occurrence onto its own identity
-- with graphwright.split_mention -- see ../identity-resolution.sql.
