-- Upstream endpoint that served the turn (spec 27), last seen across
-- its calls. Decides which endpoint's rates price the turn. Nullable:
-- rows predating the column, and turns whose provider never named an
-- endpoint. NULL renders as absence, never as a default endpoint.
ALTER TABLE turns ADD COLUMN provider TEXT;
