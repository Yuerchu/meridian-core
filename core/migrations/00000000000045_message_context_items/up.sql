-- User-selected file snapshots and command output belong to the message that
-- introduced them.  They are deliberately not message rows: they must follow
-- branch lifetime without being rendered as conversation, audited, exported,
-- or lifted to the sticky memory tail.
CREATE TABLE message_context_items (
  id TEXT PRIMARY KEY NOT NULL,
  message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('project_file', 'project_directory', 'shell_output')),
  content TEXT NOT NULL,
  display_path TEXT,
  line_start INTEGER,
  line_end INTEGER,
  content_hash TEXT NOT NULL,
  byte_count INTEGER NOT NULL,
  line_count INTEGER NOT NULL,
  token_count INTEGER NOT NULL,
  truncated INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
  metadata TEXT,
  created_at BIGINT NOT NULL,
  UNIQUE (message_id, position),
  CHECK (
    (line_start IS NULL AND line_end IS NULL)
    OR (line_start > 0 AND line_end >= line_start)
  )
);

CREATE INDEX idx_message_context_items_message
  ON message_context_items(message_id, position);
