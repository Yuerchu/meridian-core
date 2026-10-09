-- Which stage of the work a conversation is in. NULL means the default (work)
-- mode, so existing rows need no backfill and an unknown value read by an older
-- build degrades to the default rather than to a conversation with no tools.
ALTER TABLE conversations ADD COLUMN mode TEXT;

-- Artifacts a mode produces for the user to approve. Plans are the only kind
-- today; `kind` is here because the table is otherwise entirely generic and
-- adding it now costs a column, whereas adding it after release costs a data
-- migration.
--
-- Kept out of the transcript alone so the approved artifact can be re-injected
-- into the system prompt every turn, which is what lets it survive compaction.
CREATE TABLE mode_artifacts (
  id TEXT PRIMARY KEY NOT NULL,
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  kind TEXT NOT NULL DEFAULT 'plan',
  content TEXT NOT NULL,
  -- pending | approved | rejected | superseded | done
  status TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE INDEX idx_mode_artifacts_conversation ON mode_artifacts(conversation_id, kind, created_at);

-- At most one artifact of a kind is in force per conversation. Enforced here
-- rather than only inside the approve transaction, so two concurrent approvals
-- cannot both win and leave the injected artifact ambiguous. Matches how
-- todo_lists guards its single active list.
CREATE UNIQUE INDEX idx_mode_artifacts_approved
  ON mode_artifacts(conversation_id, kind) WHERE status = 'approved';
