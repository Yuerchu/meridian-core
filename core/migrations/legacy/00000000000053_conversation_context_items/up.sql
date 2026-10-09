-- A conversation dragged into the composer freezes as a context item, the way
-- an @-referenced file does.  SQLite cannot ALTER a CHECK constraint, so
-- widening `kind` to admit 'conversation' rebuilds both tables.
--
-- Built as create-copy-drop-rename, never renaming the *old* table: since
-- SQLite 3.25 RENAME rewrites other tables' REFERENCES clauses (migration
-- 24's lesson), and acp_context_deliveries points at message_context_items.
-- Dropping the old table under its own name and renaming the replacement into
-- it leaves that foreign key aimed at the new table.

CREATE TABLE message_context_items_new (
  id TEXT PRIMARY KEY NOT NULL,
  message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('project_file', 'project_directory', 'shell_output', 'conversation')),
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

INSERT INTO message_context_items_new (
  id, message_id, position, kind, content, display_path, line_start, line_end,
  content_hash, byte_count, line_count, token_count, truncated, metadata, created_at
)
SELECT
  id, message_id, position, kind, content, display_path, line_start, line_end,
  content_hash, byte_count, line_count, token_count, truncated, metadata, created_at
FROM message_context_items;

DROP TABLE message_context_items;
ALTER TABLE message_context_items_new RENAME TO message_context_items;

CREATE INDEX idx_message_context_items_message
  ON message_context_items(message_id, position);

-- The queue freezes the same items at enqueue time, so a follow-up carrying a
-- conversation reference needs the same admission.  Nothing references this
-- table, but the same construction keeps the two rebuilds identical.

CREATE TABLE queued_prompt_context_items_new (
  id TEXT PRIMARY KEY NOT NULL,
  queue_id TEXT NOT NULL REFERENCES queued_prompts(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('project_file', 'project_directory', 'conversation')),
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
  UNIQUE (queue_id, position),
  CHECK (
    (line_start IS NULL AND line_end IS NULL)
    OR (line_start > 0 AND line_end >= line_start)
  )
);

INSERT INTO queued_prompt_context_items_new (
  id, queue_id, position, kind, content, display_path, line_start, line_end,
  content_hash, byte_count, line_count, token_count, truncated, metadata, created_at
)
SELECT
  id, queue_id, position, kind, content, display_path, line_start, line_end,
  content_hash, byte_count, line_count, token_count, truncated, metadata, created_at
FROM queued_prompt_context_items;

DROP TABLE queued_prompt_context_items;
ALTER TABLE queued_prompt_context_items_new RENAME TO queued_prompt_context_items;

CREATE INDEX idx_queued_prompt_context_items_queue
  ON queued_prompt_context_items(queue_id, position);
