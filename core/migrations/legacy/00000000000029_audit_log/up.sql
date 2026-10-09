-- What was said, by whom, at what cost -- kept where deleting a conversation
-- cannot reach it.
--
-- Everything else about a turn lives under `conversations` and goes when it
-- does: `messages` cascades, `turns` cascades, the attachments on disk are
-- removed by the command itself. That is the right behaviour for a transcript,
-- which is the user's to discard. It is the wrong behaviour for a record of what
-- an operator's bot did, because the two questions have different owners: one
-- person decides what to keep in their chat list, and a different person has to
-- answer for what the deployment as a whole did last month.
--
-- So this table has no foreign keys at all. Not `ON DELETE SET NULL`, which
-- would leave rows that survive but no longer say what they were about -- the
-- one thing worse than losing a record is keeping one that has quietly become a
-- lie. Every id here is a plain TEXT column naming something that may already be
-- gone, and every fact needed to read the row is copied in beside it. There is
-- precedent: `messages.parent_id` and `messages.turn_id` are keyless for a
-- related reason (migrations 21 and 25), and `model_id` has always been a
-- snapshot rather than a reference.
--
-- Append-only by construction rather than by constraint. Nothing updates these
-- rows: an assistant reply is recorded once, when it is complete, and a message
-- that was edited produces a second row rather than replacing the first. A
-- transcript answers "what does this conversation say now"; this answers "what
-- happened", and those diverge the moment anyone edits or regenerates.
--
-- The snapshot columns are what make a row readable on its own. `source_type` /
-- `source_id` say which QQ group or private chat it came from, which today
-- requires two joins through `conversations.project_id` -- itself
-- `ON DELETE SET NULL`, so deleting a project silently erases the group a year
-- of history belonged to. `turn_origin` distinguishes desktop from bot traffic,
-- which otherwise lives only in `turns`. `sender_name` is the nickname at the
-- time: `memory_subjects` holds only the current one, and a person who renames
-- themselves would retroactively rename every line they ever wrote.
--
-- Deliberately not recorded here: tool calls and their results. A tool result is
-- our own text rather than something someone said, and the arguments can contain
-- file contents and command output that this table has no business holding a
-- second copy of. What a turn *did* is still recoverable from `turns.phase_tool`
-- while that conversation exists.
CREATE TABLE audit_messages (
  id TEXT PRIMARY KEY NOT NULL,
  -- When the record was written, which is not when the message was sent. A row
  -- recorded long after its `created_at` is an assistant reply that took a while
  -- to finish, or an import; both are worth being able to tell apart.
  recorded_at BIGINT NOT NULL,

  -- Naming things that may no longer exist. No REFERENCES clause on any of them.
  message_id TEXT NOT NULL,
  conversation_id TEXT NOT NULL,
  turn_id TEXT,

  -- Where it happened, snapshotted because the joins that would answer this
  -- today do not survive a deletion.
  source_type TEXT,
  source_id TEXT,
  turn_origin TEXT,

  role TEXT NOT NULL,
  content TEXT NOT NULL,
  sender_id BIGINT,
  sender_name TEXT,

  -- Which upstream answered and what it charged. Same contract as `messages`:
  -- the two cache figures are subsets of `input_tokens`, never additions to it.
  provider_id TEXT,
  provider_name TEXT,
  model_id TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  cache_read_tokens INTEGER,
  cache_write_tokens INTEGER,

  -- The original message's own timestamp, so a report can be built on when
  -- things were said rather than on when this table happened to hear about it.
  created_at BIGINT NOT NULL
);

-- Reports are almost always "a window of time", sometimes narrowed to one
-- person. Nothing else here is worth an index that every insert has to maintain.
CREATE INDEX idx_audit_created ON audit_messages(created_at);
CREATE INDEX idx_audit_sender ON audit_messages(sender_id) WHERE sender_id IS NOT NULL;
