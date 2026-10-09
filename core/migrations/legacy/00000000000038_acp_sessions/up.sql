-- Which agent session a hosted conversation is, so it can be picked up again.
--
-- Until now this was `acp.cwd.<conversation_id>` in `preferences`, and the
-- comment on it said what it was: a stopgap holding half the answer. The
-- directory survived a restart and the *session* did not, so reopening started
-- a brand new agent in the same folder — the transcript was all still there on
-- screen and the agent could not see a word of it. Not "had forgotten": a
-- hosted turn sends only the new message, so our transcript never enters its
-- context at all.
--
-- `session/load` is what fixes that, and it needs an id to load. Two columns is
-- the whole reason this is a table rather than a second preference: they have
-- to be written together, or a conversation ends up with a session id and no
-- directory to resume it in.
CREATE TABLE acp_sessions (
  -- One row per conversation, not per session. Resuming reuses the id and
  -- failing to resume overwrites it, so a conversation never has two live
  -- sessions — `AcpRegistry` is keyed the same way, and a table that could
  -- describe a state the registry cannot is a table describing something that
  -- does not happen.
  conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,

  -- The agent's id for the session, or NULL for "there is nothing to resume;
  -- start a new one".
  --
  -- NULL is a real state rather than a gap waiting to be filled, and it arrives
  -- two ways: a conversation from before this table existed (see the backfill
  -- below), and one whose adapter came up but never opened a session. Both mean
  -- the same thing to every caller, which is why they are not told apart.
  --
  -- Not necessarily the id that was asked for. `session/load` resumes through
  -- the SDK and answers with whatever id it actually got, so what lands here is
  -- the *reply* rather than the request.
  acp_session_id TEXT,

  -- Absolute. The adapter refuses a relative path outright, and a session means
  -- nothing without the directory it was about.
  cwd TEXT NOT NULL,

  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

-- Two conversations pointing at one agent session would be two transcripts
-- being written from the same place. Several NULLs are fine and expected —
-- SQLite does not consider them equal — which is exactly the "nothing to
-- resume" case.
CREATE UNIQUE INDEX idx_acp_sessions_session ON acp_sessions(acp_session_id);

-- Every hosted conversation that already exists, carried over with the one
-- thing the preference knew. `acp_session_id` stays NULL: the sessions those
-- rows described died with the process that made them, and inventing an id
-- would have the next reopen ask the adapter to resume something nobody ever
-- wrote down.
INSERT INTO acp_sessions (conversation_id, acp_session_id, cwd, created_at, updated_at)
SELECT substr(p.key, length('acp.cwd.') + 1), NULL, p.value, c.created_at, c.updated_at
FROM preferences p
JOIN conversations c ON c.id = substr(p.key, length('acp.cwd.') + 1)
WHERE p.key LIKE 'acp.cwd.%' AND trim(p.value) <> '';

-- And the preference goes rather than being left as a second copy. Two places
-- holding one conversation's working directory is two places that can disagree,
-- and what the loser costs is a session opened in the wrong folder.
DELETE FROM preferences WHERE key LIKE 'acp.cwd.%';
