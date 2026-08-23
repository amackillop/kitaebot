-- Wall time and outcome per turn (spec 27). Nullable: rows predating
-- these columns have no timing and render as "-" in the report.
ALTER TABLE turns ADD COLUMN started_at INTEGER;
ALTER TABLE turns ADD COLUMN duration_ms INTEGER;
ALTER TABLE turns ADD COLUMN outcome TEXT;
