-- Messages the user has written but the agent has not been given yet.
--
-- Steering already existed for delegated runs and lives in a `Mutex<HashMap>`
-- (`state.rs`), which is the right shape for a queue that may be lost: a
-- sub-agent's inbox is only meaningful while that run is going, and the run
-- goes with the process. A queue the user *stacks up* is the opposite. It is
-- typed minutes before it is needed, it is the record of what they meant to
-- happen next, and losing it silently on a kill is losing work they did.
--
-- So it is a table, and the interesting part is not the storage but the
-- ledger below it.
CREATE TABLE queued_prompts (
  id TEXT PRIMARY KEY NOT NULL,
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,

  -- What was typed. Plain text, or the same JSON parts array `messages.content`
  -- carries when there are attachments — identical either way, because this
  -- becomes a `messages` row verbatim and a second encoding would be a second
  -- thing to keep in step.
  content TEXT NOT NULL,

  -- follow_up | interject
  --
  -- The two differ in *when* the agent is given it, and the difference is the
  -- whole feature:
  --
  --   follow_up  waits for the turn to reach an ending, then starts a new one.
  --              "When you have finished all that, also do this."
  --   interject  goes in at the next point the agent accepts input — between
  --              rounds, mid-turn. "Stop, actually do it this way."
  --
  -- Claude Code offers only the second. The first is what you want far more
  -- often while something long is running, and conflating them means every
  -- queued thought interrupts.
  delivery TEXT NOT NULL,

  -- Where it sits in the queue. Rewritten wholesale on a reorder rather than
  -- being a gap-filled sequence: the list is short, a person is looking at it,
  -- and a scheme that avoids one UPDATE at the cost of an order nobody can
  -- predict is a bad trade here.
  position INTEGER NOT NULL,

  created_at BIGINT NOT NULL,

  -- ---------------------------------------------------------------- ledger
  --
  -- Four nullable timestamps, and which of them is set is the state. Written
  -- rather than derived because the process can die between any two lines of
  -- the code that would derive it — which is the entire problem this table
  -- exists to solve.
  --
  --   (none set)     queued.     Nothing has happened. Safe to deliver.
  --   dispatched_at  in flight.  Handed to a runner; the outcome is unknown.
  --   settled_at     delivered.  It became a `messages` row. Done.
  --   held_at        held.       The turn before it failed, so it is waiting
  --                              for a person rather than for a turn.
  --
  -- `dispatched_at` without `settled_at` is the one state that cannot be
  -- resolved by looking: the message may have reached the agent and caused it
  -- to run commands, or may have gone nowhere. **It is never re-delivered.**
  -- Re-sending "delete the old migration" because we are unsure whether it
  -- landed is how a queue becomes dangerous. Instead it is reported to the
  -- agent on the next turn, exactly as an interrupted turn is, and the agent —
  -- which can see the transcript and the files — decides what it means.
  --
  -- For a *native* turn this window is closable and is closed: taking the item
  -- and writing its `messages` row happen in one transaction, and every later
  -- turn resends the whole history, so a written row always reaches the model.
  -- For a *hosted* ACP session it is not: the conversation lives in the
  -- adapter's process and we do not resend history, so a row in this database
  -- proves nothing about what the agent saw. That asymmetry is why the ledger
  -- is here rather than being inferred from `messages`.
  dispatched_at BIGINT,
  dispatched_turn_id TEXT,
  settled_at BIGINT,
  settled_message_id TEXT,
  held_at BIGINT,

  -- Set when the agent has been told about an item whose delivery was in
  -- doubt. Same meaning as `turns.reported_at` and settled by the same rule:
  -- only a reply read to the end counts, so every way of getting it wrong is a
  -- repeated warning rather than a lost one.
  reported_at BIGINT
);

-- The only two questions asked of this table: what is waiting for this
-- conversation, and what does it still owe an explanation for.
CREATE INDEX idx_queued_prompts_conversation ON queued_prompts(conversation_id, position);
CREATE INDEX idx_queued_prompts_unreported ON queued_prompts(conversation_id, reported_at)
  WHERE dispatched_at IS NOT NULL AND settled_at IS NULL;
