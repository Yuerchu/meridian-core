CREATE TABLE tool_categories (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  description TEXT,
  icon TEXT,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL
);

CREATE TABLE custom_tools (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL,
  category_id TEXT REFERENCES tool_categories(id) ON DELETE SET NULL,
  parameters_schema TEXT NOT NULL DEFAULT '{"type":"object","properties":{}}',
  command TEXT NOT NULL,
  args_template TEXT,
  working_directory TEXT,
  timeout_ms INTEGER DEFAULT 30000,
  permission TEXT NOT NULL DEFAULT 'ask',
  is_enabled INTEGER NOT NULL DEFAULT 1,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE TABLE tool_presets (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  description TEXT,
  icon TEXT,
  tool_names TEXT NOT NULL,
  is_builtin INTEGER NOT NULL DEFAULT 0,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

ALTER TABLE assistants ADD COLUMN tool_preset_id TEXT REFERENCES tool_presets(id) ON DELETE SET NULL;
