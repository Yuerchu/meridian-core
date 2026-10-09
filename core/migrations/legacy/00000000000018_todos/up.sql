-- Task checklists for the agent loop. A conversation accumulates several lists
-- over its lifetime (one per phase of work); the partial unique index below is
-- what keeps exactly one of them in progress at a time, so the system prompt
-- always has a single unambiguous list to inject.
CREATE TABLE todo_lists (
  id TEXT PRIMARY KEY NOT NULL,
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  title TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'in_progress',
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE INDEX idx_todo_lists_conversation ON todo_lists(conversation_id, created_at);
CREATE UNIQUE INDEX idx_todo_lists_active ON todo_lists(conversation_id) WHERE status = 'in_progress';

-- Items are replaced wholesale on every tool call, so they carry no identity
-- beyond their position within a list.
CREATE TABLE todo_items (
  id TEXT PRIMARY KEY NOT NULL,
  list_id TEXT NOT NULL REFERENCES todo_lists(id) ON DELETE CASCADE,
  content TEXT NOT NULL,
  active_form TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending',
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL
);

CREATE INDEX idx_todo_items_list ON todo_items(list_id, sort_order);
