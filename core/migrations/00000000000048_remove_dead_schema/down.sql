ALTER TABLE conversations ADD COLUMN compact_cursor INTEGER;

CREATE TABLE attachments (
  id TEXT PRIMARY KEY NOT NULL,
  message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  file_name TEXT NOT NULL,
  file_path TEXT NOT NULL,
  mime_type TEXT NOT NULL,
  file_size BIGINT NOT NULL,
  created_at BIGINT NOT NULL
);

CREATE INDEX idx_attachments_message ON attachments(message_id);

-- Restore the intended foreign key rather than migration 24's accidental
-- reference to the already-dropped mcp_servers_old table.
CREATE TABLE tool_permissions (
  id TEXT PRIMARY KEY NOT NULL,
  tool_name TEXT NOT NULL,
  mcp_server_id TEXT REFERENCES mcp_servers(id) ON DELETE CASCADE,
  permission TEXT NOT NULL DEFAULT 'ask',
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  UNIQUE(tool_name)
);
