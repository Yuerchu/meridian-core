-- Dynamic redaction rules.  Builtins live in Rust (redaction::builtin) so they
-- can be revised on upgrade rather than frozen into a first-run seed.
--
-- (scope_type, scope_id) rather than a nullable project_id with a foreign key:
-- SQLite treats NULLs as distinct in UNIQUE, so global names could collide.
-- Same encoding as memories ('_' = global).
CREATE TABLE redaction_rules (
  id                     TEXT PRIMARY KEY NOT NULL,
  scope_type             TEXT NOT NULL CHECK (scope_type IN ('global', 'project')),
  scope_id               TEXT NOT NULL,
  name                   TEXT NOT NULL,
  description            TEXT NOT NULL,
  pattern                TEXT NOT NULL,
  category               TEXT NOT NULL CHECK (category IN ('secret', 'pii', 'network')),
  examples               TEXT NOT NULL,
  origin                 TEXT NOT NULL CHECK (origin IN ('model', 'user')),
  source_conversation_id TEXT,
  is_enabled             INTEGER NOT NULL DEFAULT 1 CHECK (is_enabled IN (0, 1)),
  created_at             BIGINT NOT NULL,
  updated_at             BIGINT NOT NULL,
  UNIQUE (scope_type, scope_id, name),
  CHECK ((scope_type = 'global' AND scope_id = '_') OR (scope_type = 'project' AND scope_id <> '_'))
);
CREATE INDEX idx_redaction_rules_scope ON redaction_rules(scope_type, scope_id, is_enabled);
