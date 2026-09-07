-- Back to a default of 1. The rows themselves stay off: which servers the user
-- chose to auto-connect is real information, and inventing a 1 for every one of
-- them would re-create the problem this migration existed to avoid.
ALTER TABLE mcp_servers RENAME TO mcp_servers_new;

CREATE TABLE mcp_servers (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  transport_type TEXT NOT NULL DEFAULT 'stdio',
  command TEXT,
  args TEXT,
  env TEXT,
  url TEXT,
  headers TEXT,
  is_enabled INTEGER NOT NULL DEFAULT 1,
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
  is_enabled, sort_order, created_at, updated_at
FROM mcp_servers_new;

DROP TABLE mcp_servers_new;
