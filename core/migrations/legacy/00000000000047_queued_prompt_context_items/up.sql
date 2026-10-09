-- Freeze @ references when they enter the durable prompt queue, not minutes
-- later when the turn starts and the file may already be different.
CREATE TABLE queued_prompt_context_items (
  id TEXT PRIMARY KEY NOT NULL,
  queue_id TEXT NOT NULL REFERENCES queued_prompts(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('project_file', 'project_directory')),
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

CREATE INDEX idx_queued_prompt_context_items_queue
  ON queued_prompt_context_items(queue_id, position);
