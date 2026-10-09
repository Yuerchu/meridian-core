-- Rebuild projects table: path nullable, add source_type/source_id/assistant_id/description
CREATE TABLE projects_new (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  path TEXT,
  source_type TEXT NOT NULL DEFAULT 'local',
  source_id TEXT,
  assistant_id TEXT REFERENCES assistants(id) ON DELETE SET NULL,
  description TEXT,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

INSERT INTO projects_new (id, name, path, source_type, source_id, assistant_id, description, created_at, updated_at)
  SELECT id, name, path, 'local', NULL, NULL, NULL, created_at, updated_at FROM projects;

DROP TABLE projects;
ALTER TABLE projects_new RENAME TO projects;

CREATE UNIQUE INDEX idx_projects_source ON projects(source_type, source_id) WHERE source_id IS NOT NULL;

-- Memories table
CREATE TABLE memories (
  id TEXT PRIMARY KEY NOT NULL,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  key TEXT NOT NULL,
  content TEXT NOT NULL,
  memory_type TEXT NOT NULL DEFAULT 'general',
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE UNIQUE INDEX idx_memories_project_key ON memories(project_id, key);
