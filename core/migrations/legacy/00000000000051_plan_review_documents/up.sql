-- A plan is a durable document, not an approval payload.  The working copy is
-- materialised under app-private storage, while these rows remain the recovery
-- truth when the process dies between a database commit and a file rename.
CREATE TABLE plan_documents (
  id TEXT PRIMARY KEY NOT NULL,
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  -- drafting | reviewing | approved | done
  state TEXT NOT NULL CHECK (state IN ('drafting', 'reviewing', 'approved', 'done')),
  -- These are intentionally not foreign keys.  Both point into
  -- plan_revisions, whose rows point back to this document, and every change is
  -- made in one transaction.  Avoiding the cycle also makes conversation
  -- deletion independent of SQLite's cascade order.
  head_revision_id TEXT,
  approved_revision_id TEXT,
  working_generation BIGINT NOT NULL DEFAULT 0 CHECK (working_generation >= 0),
  -- Relative to files/<conversation-id>; never accepted as a caller-supplied
  -- filesystem path.
  file_rel_path TEXT NOT NULL,
  lock_version BIGINT NOT NULL DEFAULT 0 CHECK (lock_version >= 0),
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE INDEX idx_plan_documents_conversation
  ON plan_documents(conversation_id, created_at);

-- One unfinished planning episode owns the conversation at a time.  `done`
-- documents remain as history and do not block a later episode.
CREATE UNIQUE INDEX idx_plan_documents_active
  ON plan_documents(conversation_id)
  WHERE state IN ('drafting', 'reviewing', 'approved');

CREATE TABLE plan_revisions (
  id TEXT PRIMARY KEY NOT NULL,
  document_id TEXT NOT NULL REFERENCES plan_documents(id) ON DELETE CASCADE,
  revision_no BIGINT NOT NULL CHECK (revision_no > 0),
  parent_revision_id TEXT,
  -- assistant | user_suggestion | legacy
  author_kind TEXT NOT NULL CHECK (author_kind IN ('assistant', 'user_suggestion', 'legacy')),
  content_markdown TEXT NOT NULL,
  content_sha256 TEXT NOT NULL CHECK (
    length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  patch TEXT,
  source_message_id TEXT,
  source_call_id TEXT,
  responding_to_suggestion_revision_id TEXT,
  -- A review projection may be retained on a sealed user suggestion.  It is
  -- not the canonical plan source.
  editor_json TEXT CHECK (editor_json IS NULL OR json_valid(editor_json)),
  editor_schema_version INTEGER,
  editor_schema_hash TEXT,
  legacy_source_artifact_id TEXT UNIQUE,
  created_at BIGINT NOT NULL,
  UNIQUE(document_id, revision_no)
);

CREATE INDEX idx_plan_revisions_document
  ON plan_revisions(document_id, revision_no);
CREATE INDEX idx_plan_revisions_content_sha
  ON plan_revisions(document_id, content_sha256);

CREATE TABLE plan_review_sessions (
  id TEXT PRIMARY KEY NOT NULL,
  document_id TEXT NOT NULL REFERENCES plan_documents(id) ON DELETE CASCADE,
  submitted_revision_id TEXT NOT NULL REFERENCES plan_revisions(id) ON DELETE CASCADE,
  turn_id TEXT,
  assistant_message_id TEXT,
  provider_call_id TEXT,
  -- native | acp | legacy
  provider_kind TEXT NOT NULL CHECK (provider_kind IN ('native', 'acp', 'legacy')),
  -- Strict typed JSON for the effective native turn destination and runtime
  -- preferences. NULL for ACP/legacy reviews, whose continuation is owned by
  -- the adapter rather than reconstructed by the native runner.
  native_runtime_config_json TEXT CHECK (
    native_runtime_config_json IS NULL OR json_valid(native_runtime_config_json)
  ),
  -- pending | approved | changes_requested | orphaned
  state TEXT NOT NULL CHECK (state IN ('pending', 'approved', 'changes_requested', 'orphaned')),
  decision_id TEXT UNIQUE,
  decision_summary TEXT,
  suggestion_revision_id TEXT,
  lock_version BIGINT NOT NULL DEFAULT 0 CHECK (lock_version >= 0),
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  decided_at BIGINT
);

CREATE INDEX idx_plan_reviews_document
  ON plan_review_sessions(document_id, created_at);
CREATE UNIQUE INDEX idx_plan_reviews_pending
  ON plan_review_sessions(document_id) WHERE state = 'pending';

CREATE TABLE plan_review_drafts (
  review_id TEXT PRIMARY KEY NOT NULL REFERENCES plan_review_sessions(id) ON DELETE CASCADE,
  base_revision_id TEXT NOT NULL REFERENCES plan_revisions(id) ON DELETE CASCADE,
  generation BIGINT NOT NULL DEFAULT 0 CHECK (generation >= 0),
  mode TEXT NOT NULL CHECK (mode IN ('rich', 'source')),
  base_editor_json TEXT CHECK (base_editor_json IS NULL OR json_valid(base_editor_json)),
  draft_editor_json TEXT CHECK (draft_editor_json IS NULL OR json_valid(draft_editor_json)),
  base_normalized_markdown TEXT NOT NULL,
  draft_normalized_markdown TEXT NOT NULL,
  -- Exact source-mode draft.  NULL in rich mode; unlike normalized Markdown it
  -- is never parsed and re-serialised before being restored.
  source_text TEXT,
  editor_schema_version INTEGER,
  editor_schema_hash TEXT,
  global_note TEXT,
  selection_json TEXT CHECK (selection_json IS NULL OR json_valid(selection_json)),
  draft_sha256 TEXT NOT NULL CHECK (
    length(draft_sha256) = 64 AND draft_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  CHECK (
    (mode = 'rich' AND source_text IS NULL) OR
    (mode = 'source' AND source_text IS NOT NULL)
  )
);

CREATE TABLE plan_comments (
  id TEXT PRIMARY KEY NOT NULL,
  review_id TEXT NOT NULL REFERENCES plan_review_sessions(id) ON DELETE CASCADE,
  position INTEGER NOT NULL CHECK (position >= 0),
  -- draft is being composed; submitted is immutable review history.
  state TEXT NOT NULL CHECK (state IN ('draft', 'active', 'orphaned', 'submitted', 'deleted')),
  anchor_kind TEXT NOT NULL CHECK (anchor_kind IN ('rich', 'source')),
  anchor_json TEXT NOT NULL CHECK (json_valid(anchor_json)),
  body TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  UNIQUE(review_id, position)
);

CREATE INDEX idx_plan_comments_review
  ON plan_comments(review_id, position);

CREATE TABLE plan_review_deliveries (
  id TEXT PRIMARY KEY NOT NULL,
  review_id TEXT NOT NULL REFERENCES plan_review_sessions(id) ON DELETE CASCADE,
  target TEXT NOT NULL CHECK (target IN ('native', 'acp')),
  -- queued | dispatched | acknowledged | held | in_doubt
  state TEXT NOT NULL CHECK (state IN ('queued', 'dispatched', 'acknowledged', 'held', 'in_doubt')),
  payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
  attempt_token TEXT,
  target_session_id TEXT,
  target_turn_id TEXT,
  error TEXT,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  dispatched_at BIGINT,
  acknowledged_at BIGINT,
  held_at BIGINT,
  UNIQUE(review_id, target)
);

CREATE INDEX idx_plan_deliveries_state
  ON plan_review_deliveries(state, created_at);

CREATE TABLE plan_materializations (
  id TEXT PRIMARY KEY NOT NULL,
  document_id TEXT NOT NULL REFERENCES plan_documents(id) ON DELETE CASCADE,
  revision_id TEXT NOT NULL REFERENCES plan_revisions(id) ON DELETE CASCADE,
  generation BIGINT NOT NULL CHECK (generation > 0),
  expected_sha256 TEXT CHECK (
    expected_sha256 IS NULL OR
    (length(expected_sha256) = 64 AND expected_sha256 NOT GLOB '*[^0-9a-f]*')
  ),
  desired_sha256 TEXT NOT NULL CHECK (
    length(desired_sha256) = 64 AND desired_sha256 NOT GLOB '*[^0-9a-f]*'
  ),
  -- pending | applied | conflict
  state TEXT NOT NULL CHECK (state IN ('pending', 'applied', 'conflict')),
  -- Set only by the explicit "restore from database" action.  An initial
  -- missing file also has expected_sha256 NULL, so NULL alone cannot authorise
  -- replacing unexpected external bytes.
  force_replace INTEGER NOT NULL DEFAULT 0 CHECK (force_replace IN (0, 1)),
  error TEXT,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  applied_at BIGINT,
  UNIQUE(document_id, generation)
);

CREATE INDEX idx_plan_materializations_pending
  ON plan_materializations(state, created_at);
