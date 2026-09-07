DROP TABLE IF EXISTS memories;

CREATE TABLE projects_old (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  path TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

INSERT INTO projects_old SELECT id, name, COALESCE(path, ''), created_at, updated_at FROM projects;
DROP TABLE projects;
ALTER TABLE projects_old RENAME TO projects;
