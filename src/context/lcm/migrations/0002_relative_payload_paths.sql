-- Hinted paths are always workspace-relative (PathGuard rejects absolute
-- file_read arguments), so absolute rows are exactly the no-hint fallback.
UPDATE large_files
SET path = 'context/lcm/payloads/' || file_id
WHERE path LIKE '/%';
