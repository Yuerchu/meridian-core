-- Typed failures and warnings a hosted Claude Code session reported about
-- itself, through the AIR `sessionFailure` extension.
--
-- One row per incident, not per event: the adapter publishes the same
-- `notice_id` again with a higher `revision` when an incident changes (a retry
-- warning becoming the terminal failure), and the client keeps the latest
-- revision in place. A lower or equal revision is ignored on write.
--
-- `turn_id` carries no foreign key, like `messages.turn_id`: a session-scoped
-- incident (an expired login noticed between turns, a replayed history error)
-- has no turn and is NULL. `actions` is the adapter's ordered recommendation
-- (`retry` / `login` / `new_session`) as a JSON array; the app decides which
-- of them it can offer.
CREATE TABLE acp_session_notices (
  id              TEXT PRIMARY KEY NOT NULL,
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  turn_id         TEXT,
  notice_id       TEXT NOT NULL,
  revision        INTEGER NOT NULL CHECK (revision >= 0),
  category        TEXT NOT NULL CHECK (category IN ('connection', 'access', 'limit', 'request', 'service', 'unknown')),
  severity        TEXT NOT NULL CHECK (severity IN ('warning', 'error')),
  title           TEXT NOT NULL,
  details         TEXT,
  reason          TEXT,
  actions         TEXT NOT NULL CHECK (json_valid(actions) AND json_type(actions) = 'array'),
  created_at      BIGINT NOT NULL,
  updated_at      BIGINT NOT NULL,
  UNIQUE (conversation_id, notice_id)
);
CREATE INDEX idx_acp_session_notices_conversation ON acp_session_notices(conversation_id, created_at);
