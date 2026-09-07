-- Layered memory. The single `project_id` anchor becomes (scope_type, scope_id),
-- so a memory can hang off a project, off the bot itself, or off one person.
--
-- The foreign key to projects is dropped on purpose: scope_id is polymorphic and
-- SQLite has no conditional foreign keys. Orphan cleanup for the 'project' scope
-- moves into db::ops::project::delete_project plus a startup sweep.
CREATE TABLE memories_new (
  id                TEXT PRIMARY KEY NOT NULL,
  scope_type        TEXT NOT NULL,             -- 'project' | 'onebot_global' | 'onebot_user'
  scope_id          TEXT NOT NULL,             -- project uuid | '_' | 'onebot:12345'
  key               TEXT NOT NULL,
  content           TEXT NOT NULL,
  memory_type       TEXT NOT NULL DEFAULT 'general',
  -- Who this memory is about. Required for onebot_user scope; optional on
  -- project scope so opt-out can find group memories that name a person.
  subject_scope_id  TEXT,
  -- Where it was learned. Drives the injection gate: what was learned in a
  -- private chat must never surface in a group.
  origin            TEXT NOT NULL DEFAULT 'desktop',   -- private|group|admin|desktop|legacy
  -- Separate from origin: the operator's private annotations are invisible to
  -- their subject, but an operator-approved bot rule is not.
  visibility        TEXT NOT NULL DEFAULT 'normal',    -- normal|owner_only
  source_session_id TEXT,
  deleted_at        BIGINT,
  deleted_by        TEXT,                      -- 'self' | 'admin' | 'lru'
  created_at        BIGINT NOT NULL,
  updated_at        BIGINT NOT NULL
);

-- Existing rows all hang off a project. OneBot private-chat memories move to the
-- onebot_user scope: leaving them on a project would keep them invisible to
-- /memory me and unreachable by opt-out, which is the hole this migration closes.
-- projects.source_type/source_id is enough to do this attribution safely.
INSERT INTO memories_new (id, scope_type, scope_id, key, content, memory_type,
                          subject_scope_id, origin, visibility, source_session_id,
                          deleted_at, deleted_by, created_at, updated_at)
SELECT m.id,
       CASE WHEN p.source_type = 'onebot_private' THEN 'onebot_user' ELSE 'project' END,
       CASE WHEN p.source_type = 'onebot_private' THEN 'onebot:' || p.source_id ELSE m.project_id END,
       m.key, m.content, m.memory_type,
       CASE WHEN p.source_type = 'onebot_private' THEN 'onebot:' || p.source_id END,
       -- Old group memories carry no trustworthy sender, so they are marked
       -- 'legacy' rather than 'group': they must not pass as evidence produced
       -- by the new identity pipeline.
       CASE p.source_type WHEN 'onebot_private' THEN 'private'
                          WHEN 'onebot_group'   THEN 'legacy'
                          ELSE 'desktop' END,
       'normal', NULL, NULL, NULL, m.created_at, m.updated_at
FROM memories m
JOIN projects p ON p.id = m.project_id;

DROP TABLE memories;
ALTER TABLE memories_new RENAME TO memories;

-- Partial index: a soft-deleted key must be creatable again. A plain unique
-- index would force upsert to resurrect tombstones, which would break both the
-- trash view and undo.
CREATE UNIQUE INDEX idx_memories_scope_key
  ON memories(scope_type, scope_id, key) WHERE deleted_at IS NULL;
CREATE INDEX idx_memories_subject
  ON memories(subject_scope_id) WHERE deleted_at IS NULL;

-- One row per remembered person. Carries only the LRU clock and per-person
-- flags; the memories themselves stay in `memories` so the per-subject cap and
-- the injection query hit a single table.
CREATE TABLE memory_subjects (
  scope_id     TEXT PRIMARY KEY NOT NULL,      -- same encoding as memories.scope_id
  display_name TEXT,
  last_seen_at BIGINT NOT NULL,                -- interaction clock, not write clock
  created_at   BIGINT NOT NULL,
  is_protected INTEGER NOT NULL DEFAULT 0,     -- mirrors admin_users, self-healing
  is_pinned    INTEGER NOT NULL DEFAULT 0,     -- manual exemption, capped separately
  opted_out    INTEGER NOT NULL DEFAULT 0      -- refuses to be remembered
);

-- Eviction scans oldest-first; without this every write would sort the table.
CREATE INDEX idx_memory_subjects_last_seen ON memory_subjects(last_seen_at);

-- Bot-wide memory proposals awaiting operator approval. A Mutex map cannot hold
-- these: the approval window outlives the process.
--
-- AUTOINCREMENT, not a plain INTEGER PRIMARY KEY: the latter reuses the highest
-- rowid after a delete, so a stale "同意 N" could approve a brand new proposal.
CREATE TABLE memory_proposals (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  key            TEXT NOT NULL,
  content        TEXT NOT NULL,
  memory_type    TEXT NOT NULL DEFAULT 'general',
  origin_session TEXT,
  proposer_id    BIGINT,
  status         TEXT NOT NULL DEFAULT 'pending',   -- pending|approved|rejected|expired
  created_at     BIGINT NOT NULL,
  expires_at     BIGINT NOT NULL,
  resolved_at    BIGINT,
  resolved_by    BIGINT
);

CREATE INDEX idx_memory_proposals_status ON memory_proposals(status, expires_at);

-- Trustworthy speaker attribution for group history. Nullable: desktop and
-- compaction rows have no sender.
ALTER TABLE messages ADD COLUMN sender_id BIGINT;
