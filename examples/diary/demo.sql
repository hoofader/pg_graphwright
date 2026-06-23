-- Diary walkthrough. Run examples/diary/schema.sql first (in its own
-- session, so the database-level extractor setting is in effect here).
--
--   psql -f examples/diary/schema.sql
--   psql -f examples/diary/demo.sql
--
-- Two diarists, Emma and Jack. Each writes only their own entries and sees
-- only their own knowledge graph -- enforced by row-level security, the same
-- boundary that protects the diary text itself.

-- Run this as the extension owner (the role that created it): it calls the
-- owner-only maintenance/review functions and SET ROLEs into the diarists.
DROP ROLE IF EXISTS emma;
DROP ROLE IF EXISTS jack;
CREATE ROLE emma;
CREATE ROLE jack;

DELETE FROM diary;  -- a clean slate if the demo is re-run

-- Emma's diary. She is a little inconsistent: her friend Sara also turns up
-- as "Sarah". She sees Tom often and recently met Lucy. Entries are inserted
-- AS emma, so the diarist column and the row-security policy line up.
SET ROLE emma;
INSERT INTO diary (entry) VALUES
    ('Another long day at work. I left early to meet Sara at the coffee place we always go to. She brought Tom along, and the three of us talked for hours.'),
    ('Quiet morning at home. Sarah called while I was making tea, and we ended up talking for almost an hour about her sister. She keeps saying I should finally meet Lucy.'),
    ('After work I took a long walk with Tom. He has been weighing a new job and was not sure what to do. We ran into Lucy on the way back, and the three of us got dinner.');
RESET ROLE;

-- Jack keeps his own diary, a separate world.
SET ROLE jack;
INSERT INTO diary (entry) VALUES
    ('Busy week. Sara from the office stopped by my desk to go over some numbers, and we spent twenty minutes complaining about the new system. After that I grabbed lunch with Daniel.'),
    ('I stayed in tonight. Daniel sent a long voice message about his weekend trip. He sounded tired but in good spirits.');
RESET ROLE;

-- Build the graph. In production the background worker does this on a tick;
-- here we run it once. maintain() runs as the owner over every row.
SELECT graphwright.maintain();

-- Emma opens her app. She queries plain views; row security makes the results
-- hers alone. Sara and Sarah are still two nodes (the same short name spelled
-- two ways), and the app surfaces them as a review suggestion.
SET ROLE emma;
SELECT person FROM my_people ORDER BY person;              -- Lucy, Sara, Sarah, Tom
SELECT person, also FROM my_circle ORDER BY person, also;  -- her co-mentions
SELECT * FROM my_review_queue;                             -- Sara <-> Sarah ?
RESET ROLE;

-- Jack sees ONLY his diary's graph. Emma's Tom/Lucy/Sarah do not exist for
-- him, and a DIRECT catalog read is filtered the same way -- the views are no
-- privileged back door.
SET ROLE jack;
SELECT person FROM my_people ORDER BY person;              -- Daniel, Sara
SELECT count(*) AS entities_jack_can_see FROM graphwright.entity;
RESET ROLE;

-- Emma confirms the suggestion. The app applies it on her behalf (a
-- privileged action: the review functions run as the owner). The decision is
-- durable and replayed on every re-resolve.
SELECT graphwright.merge('diary', 'Sara', 'Sarah');
SELECT graphwright.maintain();
SET ROLE emma;
SELECT person FROM my_people ORDER BY person;              -- Sara and Sarah now one
RESET ROLE;
-- ...and it is reversible -- graphwright.unmerge('diary','Sara','Sarah').

-- Live: Emma edits an entry, and the graph tracks the change on the next
-- maintenance tick, like any index.
SET ROLE emma;
UPDATE diary SET entry = entry || ' Ben joined us later.'
    WHERE entry LIKE 'After work I took a long walk%';
RESET ROLE;
SELECT graphwright.maintain();
SET ROLE emma;
SELECT person FROM my_people ORDER BY person;              -- Ben has joined
RESET ROLE;

-- A query a diary app would actually run: Emma's most-connected people.
SET ROLE emma;
SELECT person, count(*) AS times_together
FROM (SELECT person FROM my_circle UNION ALL SELECT also FROM my_circle) t
GROUP BY person
ORDER BY times_together DESC, person;
RESET ROLE;

-- Note on scope: identity is resolved globally by name (Emma's Sara and
-- Jack's Sara are the same NODE), but visibility is per diarist. To keep two
-- people who share a name apart, split one occurrence onto its own identity
-- with graphwright.split_mention -- see ../identity-resolution.sql.
