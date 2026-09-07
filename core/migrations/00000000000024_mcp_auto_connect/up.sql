-- `is_enabled` had no meaning: nothing read it, no UI set it, and every row
-- carried the column default of 1. It now means "connect this server when the
-- app starts", which makes the existing value actively wrong — leaving it would
-- have the first launch after this upgrade spawn every MCP server ever
-- configured, including the misconfigured ones and the ones whose command
-- downloads half of npm before it fails.
--
-- Everything starts off. The user turns on the servers they want, once they
-- know each one works.
UPDATE mcp_servers SET is_enabled = 0;

-- The default follows the same rule for rows created from here on. The command
-- layer passes an explicit 0 as well; this keeps a direct insert honest.
ALTER TABLE mcp_servers RENAME TO mcp_servers_old;

CREATE TABLE mcp_servers (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  transport_type TEXT NOT NULL DEFAULT 'stdio',
  command TEXT,
  args TEXT,
  env TEXT,
  url TEXT,
  headers TEXT,
  is_enabled INTEGER NOT NULL DEFAULT 0,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

INSERT INTO mcp_servers (
  id, name, transport_type, command, args, env, url, headers,
  is_enabled, sort_order, created_at, updated_at
)
SELECT
  id, name, transport_type, command, args, env, url, headers,
  0, sort_order, created_at, updated_at
FROM mcp_servers_old;

DROP TABLE mcp_servers_old;
