CREATE TABLE projects (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  path TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE UNIQUE INDEX idx_projects_path ON projects(path);

ALTER TABLE conversations ADD COLUMN project_id TEXT REFERENCES projects(id) ON DELETE SET NULL;
