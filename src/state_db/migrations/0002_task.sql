-- Task attribution (spec 27): the unit-of-work key a turn bills to.
-- Nullable: rows predating the column have no task and render in the
-- report's (untracked) bucket.
ALTER TABLE turns ADD COLUMN task TEXT;
