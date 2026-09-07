DROP TABLE IF EXISTS memory_proposals;
DROP INDEX IF EXISTS idx_memory_subjects_last_seen;
DROP TABLE IF EXISTS memory_subjects;

CREATE TABLE memories_old (
  id TEXT PRIMARY KEY NOT NULL,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  key TEXT NOT NULL,
  content TEXT NOT NULL,
  memory_type TEXT NOT NULL DEFAULT 'general',
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

-- Only project-scoped live rows have somewhere to land. Bot-wide memories are
-- dropped outright; per-person memories are mapped back onto the OneBot private
-- project they came from, when one still exists.
INSERT INTO memories_old (id, project_id, key, content, memory_type, created_at, updated_at)
SELECT m.id, m.scope_id, m.key, m.content, m.memory_type, m.created_at, m.updated_at
FROM memories m
WHERE m.scope_type = 'project' AND m.deleted_at IS NULL;

INSERT INTO memories_old (id, project_id, key, content, memory_type, created_at, updated_at)
SELECT m.id, p.id, m.key, m.content, m.memory_type, m.created_at, m.updated_at
FROM memories m
JOIN projects p
  ON p.source_type = 'onebot_private'
 AND 'onebot:' || p.source_id = m.scope_id
WHERE m.scope_type = 'onebot_user' AND m.deleted_at IS NULL;

DROP TABLE memories;
ALTER TABLE memories_old RENAME TO memories;
CREATE UNIQUE INDEX idx_memories_project_key ON memories(project_id, key);

-- messages.sender_id is left in place: dropping a column means rebuilding the
-- messages table, which is far riskier than leaving a nullable unused column.
