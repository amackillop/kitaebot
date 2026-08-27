-- Prompt-cache hits per turn (spec 27). Nullable three ways: rows
-- predating the column, and turns whose provider never reported
-- prompt_tokens_details. NULL renders as "-", never zero.
ALTER TABLE turns ADD COLUMN cached_tokens INTEGER;
