-- The directory goes back where it came from. The session id cannot: the shape
-- this returns to has nowhere to put it, which is the reason for the migration.
INSERT OR REPLACE INTO preferences (key, value, updated_at)
SELECT 'acp.cwd.' || conversation_id, cwd, updated_at FROM acp_sessions;

DROP TABLE acp_sessions;
