-- A turn has never existed anywhere but on the stack of the function running
-- it. `TurnCoordinator` is a HashMap behind a Mutex, `ApprovalWaiters` holds
-- oneshot senders, and both go with the process. So when the app is killed the
-- rows it leaves behind cannot be read back honestly:
--
--   * an assistant row with empty content is either a model that returned
--     nothing or a stream that died mid-answer — and the window between
--     creating that row and filling it in is the longest in the turn;
--   * a tool call with no answering row is either one that never ran, one that
--     was sitting in front of the user waiting to be approved, or one that ran
--     to completion — wrote the file, executed the command — and died before
--     the result could be recorded.
--
-- Those are very different things to tell someone, and the last of them is the
-- one that matters: the side effects already happened. This table is where a
-- turn writes down what it is about to do *before* doing it, so whatever the
-- last phase says is where it died.
CREATE TABLE turns (
  id TEXT PRIMARY KEY NOT NULL,
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,

  -- desktop | onebot
  origin TEXT NOT NULL,

  -- running | done | cancelled | failed | interrupted
  --
  -- `running` can only ever be true of the process that wrote it: turns do not
  -- survive a restart. A `running` row found at startup is therefore, by
  -- definition, a turn that was interrupted, and startup reconciliation says
  -- so. That is also why nothing needs to be written from a destructor —
  -- leaving the row at `running` *is* the record that the turn never reached
  -- its own ending, and destructors do not run for a kill anyway.
  status TEXT NOT NULL,

  -- streaming | awaiting_approval | running_tool | compacting
  --
  -- Only meaningful for a turn that did not end normally: it is the last thing
  -- the turn admitted to before it stopped. `running_tool` is the one worth
  -- reading carefully, because it is the one where the world outside the
  -- database may already have changed.
  phase TEXT,

  -- The tool `phase` refers to, when it refers to one.
  phase_tool TEXT,

  -- Why the turn failed, for `failed`. Not a user-facing string; it is what
  -- the run reported.
  error TEXT,

  started_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  ended_at BIGINT
);

-- Reading a conversation's turns in the order they happened is the only access
-- pattern: the newest one decides what to tell the model, and the whole list
-- feeds the transcript.
CREATE INDEX idx_turns_conversation ON turns(conversation_id, started_at);

-- Which turn wrote this row.
--
-- Deliberately a plain TEXT column with no REFERENCES clause, for the same
-- reason `parent_id` has none (migration 21): foreign keys are enforced
-- immediately, and a conversation delete removes turns and messages in the
-- same cascade with no ordering guarantee between them, so a live constraint
-- here would trip on rows that are about to disappear anyway. Nothing depends
-- on the edge being enforced — a message whose turn is missing simply reads as
-- one written before this migration.
--
-- Left NULL on compaction summaries. A summary stands in for history rather
-- than being something a turn produced, and it survives the turns it replaced.
ALTER TABLE messages ADD COLUMN turn_id TEXT;

-- How a tool call ended, on the `tool` row that answers it: success | denied |
-- error.
--
-- NULL reads as success, so existing rows keep behaving exactly as they do
-- today and nothing needs backfilling. Without this column a refusal comes back
-- from the database indistinguishable from a result — the transcript shows a
-- green tick with the refusal text sitting inside it as though the tool had
-- produced it. Today that is papered over by in-memory session state, which is
-- to say it is correct until the window is reloaded.
ALTER TABLE messages ADD COLUMN tool_outcome TEXT;
