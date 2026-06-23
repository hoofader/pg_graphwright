-- pg_graphwright example: cross-script resolution, with a human who can
-- always overrule it, reversibly.
--
-- Distinctive names that share a pronunciation auto-merge across scripts;
-- short, ambiguous ones are offered for review instead of merged; and every
-- decision the system or a human makes is durable AND reversible after the
-- fact (apply-then-review, SAGA-style). Nothing waits for a human, but a
-- human can correct anything.
--
--   psql -f examples/identity-resolution.sql   (against an installed extension)

CREATE EXTENSION IF NOT EXISTS pg_graphwright;
RESET graphwright.extractor;  -- use the built-in tokenizer: one logged name per row

DROP TABLE IF EXISTS contacts CASCADE;

-- An international contact desk: field staff log a name in whatever script
-- they typed.
CREATE TABLE contacts (id int PRIMARY KEY, desk text, mention text);
INSERT INTO contacts VALUES
  (1, 'tehran', 'خشایار'),  (2, 'london', 'Khashayar'),
  (3, 'moscow', 'Хабаров'),  (4, 'kyiv',  'Khabarov'),
  (5, 'tehran', 'علی'),      (6, 'london', 'Ali');
CREATE INDEX contacts_kg ON contacts USING graphwright (mention);
SELECT graphwright.maintain();

-- Distinctive cross-script names auto-merge though they share no characters
-- (same consonant skeleton): Khashayar~خشایار and Khabarov~Хабаров each
-- become ONE entity. Short, ambiguous names do not auto-merge.
SELECT surface FROM graphwright.entities('contacts') ORDER BY surface;

-- The short pair is offered for review, not merged:
SELECT surface_a, surface_b FROM graphwright.proposals('contacts');   -- Ali / علی

-- A human confirms it. The decision is durable (replayed on every re-resolve):
SELECT graphwright.merge('contacts', 'Ali', 'علی');
SELECT graphwright.maintain();
SELECT surface FROM graphwright.entities('contacts') ORDER BY surface;  -- Ali+علی now one

-- ...and reversible:
SELECT graphwright.unmerge('contacts', 'Ali', 'علی');
SELECT graphwright.maintain();

-- The hard case: two different people both logged identically as 'Sara'. The
-- exact stage folds them into one entity, which is sometimes wrong.
INSERT INTO contacts VALUES (7, 'berlin', 'Sara'), (8, 'paris', 'Sara');
SELECT graphwright.maintain();
SELECT count(*) AS sara_entities FROM graphwright.entities('contacts') WHERE surface = 'sara';  -- 1

-- Separate one occurrence onto its own identity. split_mention pins it by the
-- source row (its ctid here); read that from mentions() rather than guessing.
SELECT (ctid)::text AS pk8 FROM contacts WHERE id = 8 \gset
SELECT graphwright.split_mention('contacts', :'pk8', 'Sara');
SELECT graphwright.maintain();
SELECT count(*) AS sara_entities FROM graphwright.entities('contacts') WHERE surface = 'sara';  -- 2

-- And that, too, is reversible: drop the override and they fold back.
SELECT graphwright.unsplit_mention('contacts', :'pk8', 'Sara');
SELECT graphwright.maintain();
SELECT count(*) AS sara_entities FROM graphwright.entities('contacts') WHERE surface = 'sara';  -- 1

-- The full audit log of human decisions on this table:
SELECT * FROM graphwright.decisions('contacts');
