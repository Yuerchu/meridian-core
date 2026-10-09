-- A conversation that exists because a turn delegated part of its work.
--
-- The sub-agent runs a full turn of its own: it streams, calls tools, asks for
-- approval, can be interrupted. All of that has to be written down somewhere,
-- and the only shape that already survives a crash is a conversation. So a
-- sub-agent gets one -- with its own head, its own turns, its own transcript --
-- and the parent's transcript keeps only what the model needs to see: one tool
-- call and one result. The fifteen rounds in between never enter the parent's
-- context.
--
-- `parent_conversation_id IS NOT NULL` is the whole test for "this is a
-- sub-agent's conversation". It hides the row from the sidebar; the only way in
-- is the card on the parent's turn that spawned it.
--
-- No foreign key, for the same reason `messages.parent_id` has none
-- (migration 21): a per-row check on a column that is deliberately allowed to
-- dangle costs more than it protects, and the cascade order during a delete is
-- not something we get to choose.
ALTER TABLE conversations ADD COLUMN parent_conversation_id TEXT;

-- Which tool call spawned it, as a pair.
--
-- The message id is not redundant. Provider call ids repeat: a gateway that
-- numbers per request sends `"0"` for the first tool call of every response, so
-- one parent conversation delegating twice produces two rows claiming the same
-- `spawned_by_call_id`. Matching on the call id alone hangs the second run's
-- card on the first run's tool call. The same repetition is why approvals grew
-- their own identity in migration-era `ApprovalWaiters`; this is that lesson
-- applied to a second place.
ALTER TABLE conversations ADD COLUMN spawned_by_message_id TEXT;
ALTER TABLE conversations ADD COLUMN spawned_by_call_id TEXT;

-- The delegated run itself, pinned.
--
-- The parent's card reports on this turn and no other. A sub-agent's
-- conversation stays writable after the run finishes -- the user can open it
-- and keep asking -- and those follow-ups are turns in the same conversation.
-- Reading "the latest turn" would let an unrelated chat two days later decide
-- what the parent's card says about a run that ended long ago.
ALTER TABLE conversations ADD COLUMN spawned_turn_id TEXT;

-- Which built-in agent it is: `explore` (read-only) or `agent` (inherits the
-- main assistant's tools). Stored lowercase and without a CHECK, like every
-- other enum here, so an unknown value degrades instead of failing a write.
ALTER TABLE conversations ADD COLUMN agent_kind TEXT;

-- The model the sub-agent actually ran on.
--
-- Ordinary conversations do not record this -- the picker is frontend state and
-- never reaches the database -- so these stay NULL for them and nothing
-- changes. A sub-agent's conversation needs it because it outlives the run: the
-- user can reopen it and continue, and a transcript that switches models
-- halfway with nothing marking where is one nobody can reason about. It also
-- feeds the context indicator and manual compaction, which would otherwise size
-- themselves against the parent's window.
ALTER TABLE conversations ADD COLUMN agent_provider_id TEXT;
ALTER TABLE conversations ADD COLUMN agent_model_id TEXT;

-- The second ledger for interruption reports.
--
-- A delegated run that was cut off has two audiences. The parent has to be told
-- -- "a sub-agent was partway through edit_file, its result is unknown" is the
-- whole point of recording phases -- and the sub-agent's own conversation has
-- to be told too, because the user can open it and carry on from there.
--
-- One `reported_at` cannot serve both. Whoever reads a reply to the end first
-- would clear it for everyone, so a user who opens the child, says one thing,
-- and goes back to the parent would find the parent permanently unable to learn
-- that a file may have been half-written. `reported_at` therefore means "the
-- conversation this turn belongs to has been told", and this column means "the
-- conversation that spawned it has been told".
--
-- Two columns rather than a `(turn_id, recipient, reported_at)` table because
-- the depth is one by construction: a sub-agent is handed no `run_agent` tool,
-- so a grandchild cannot be written. If nesting is ever opened up, this column
-- has to become that table.
ALTER TABLE turns ADD COLUMN parent_reported_at BIGINT;
