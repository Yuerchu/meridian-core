-- What set a turn going, beside `origin`, which says which runner wrote it.
--
-- A turn a background task woke, or one a hosted agent started on its own, has
-- no question above it. The transcript groups turns by their question, so
-- without this such a turn was folded into the one before: its cost, its
-- failure and its closing sentence were all reported against a turn that had
-- already ended.
--
-- 'user' is what every existing row was, including a plan continuation, which
-- the transcript keeps with the question it continues either way.
--
-- `trigger_ref` names what did the waking: a background task's id, or the
-- agent's own id for one. Empty for anything a person started.
ALTER TABLE turns ADD COLUMN trigger TEXT NOT NULL DEFAULT 'user'
    CHECK (trigger IN ('user', 'plan_continuation', 'task_completion', 'agent_autonomous'));
ALTER TABLE turns ADD COLUMN trigger_ref TEXT;
