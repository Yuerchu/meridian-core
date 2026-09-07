-- Meridian initial schema

CREATE TABLE providers (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  provider_type TEXT NOT NULL DEFAULT 'openai',
  base_url TEXT NOT NULL,
  is_enabled INTEGER NOT NULL DEFAULT 1,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE TABLE assistants (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  description TEXT,
  avatar TEXT,
  system_prompt TEXT NOT NULL DEFAULT '',
  provider_id TEXT REFERENCES providers(id) ON DELETE SET NULL,
  model_id TEXT,
  temperature REAL,
  top_p REAL,
  max_tokens INTEGER,
  is_default INTEGER NOT NULL DEFAULT 0,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE TABLE conversations (
  id TEXT PRIMARY KEY NOT NULL,
  title TEXT,
  assistant_id TEXT REFERENCES assistants(id) ON DELETE SET NULL,
  is_pinned INTEGER NOT NULL DEFAULT 0,
  is_archived INTEGER NOT NULL DEFAULT 0,
  message_count INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE INDEX idx_conversations_updated ON conversations(updated_at DESC);
CREATE INDEX idx_conversations_pinned ON conversations(is_pinned DESC, updated_at DESC);

CREATE TABLE messages (
  id TEXT PRIMARY KEY NOT NULL,
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  role TEXT NOT NULL,
  content TEXT NOT NULL DEFAULT '',
  provider_id TEXT REFERENCES providers(id) ON DELETE SET NULL,
  model_id TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  tool_calls TEXT,
  tool_call_id TEXT,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL
);

CREATE INDEX idx_messages_conversation ON messages(conversation_id, sort_order);

CREATE TRIGGER trg_messages_sort_order
AFTER INSERT ON messages
FOR EACH ROW
WHEN NEW.sort_order = 0
BEGIN
  UPDATE messages SET sort_order = (
    SELECT COALESCE(MAX(sort_order), 0) + 1
    FROM messages WHERE conversation_id = NEW.conversation_id
  ) WHERE id = NEW.id;
END;

CREATE TRIGGER trg_messages_count_insert
AFTER INSERT ON messages
BEGIN
  UPDATE conversations SET message_count = message_count + 1, updated_at = NEW.created_at
  WHERE id = NEW.conversation_id;
END;

CREATE TRIGGER trg_messages_count_delete
AFTER DELETE ON messages
BEGIN
  UPDATE conversations SET message_count = message_count - 1
  WHERE id = OLD.conversation_id;
END;

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

CREATE TABLE mcp_servers (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  transport_type TEXT NOT NULL DEFAULT 'stdio',
  command TEXT,
  args TEXT,
  env TEXT,
  url TEXT,
  is_enabled INTEGER NOT NULL DEFAULT 1,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE TABLE tool_permissions (
  id TEXT PRIMARY KEY NOT NULL,
  tool_name TEXT NOT NULL,
  mcp_server_id TEXT REFERENCES mcp_servers(id) ON DELETE CASCADE,
  permission TEXT NOT NULL DEFAULT 'ask',
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  UNIQUE(tool_name)
);

CREATE TABLE preferences (
  key TEXT PRIMARY KEY NOT NULL,
  value TEXT NOT NULL,
  updated_at BIGINT NOT NULL
);
