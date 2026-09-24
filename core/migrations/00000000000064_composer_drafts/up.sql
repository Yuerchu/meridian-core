-- What somebody has typed into a composer and not sent yet.
--
-- It used to be React state, so switching conversation remounted the composer
-- and the draft was gone, and a crash took every unsent draft with it. The
-- app is meant to hold no state of its own that the database does not also
-- hold, and half a message is state. `queued_prompts` already made the same
-- call for text that has been submitted but not delivered; this is the step
-- before it.
--
-- One row per composer. `slot` is the key, and there are exactly two shapes of
-- it, pinned by the CHECK below so they cannot drift from `conversation_id`:
--
--   'new'                      the welcome composer, before any conversation
--                              exists. There is one on screen, so there is
--                              one slot; it is not per project, because the
--                              project picker changes where the conversation
--                              will be filed, not what was typed.
--   'conversation:<id>'        a conversation's composer.
--
-- A string key rather than a nullable `conversation_id` primary key: SQLite
-- lets a TEXT primary key hold NULL, but only one NULL is not something UNIQUE
-- promises, and an upsert needs a conflict target that is never NULL.
CREATE TABLE composer_drafts (
  slot TEXT PRIMARY KEY NOT NULL,

  -- Carried beside the slot so the row goes with its conversation through the
  -- foreign key — the same way `acp_sessions` and `queued_prompts` do — rather
  -- than through a clause somebody has to remember in `delete_conversation`.
  conversation_id TEXT UNIQUE REFERENCES conversations(id) ON DELETE CASCADE,

  -- The text exactly as it sits in the field, `/` and `!` prefixes included:
  -- those are decided when it is sent, not while it is being typed.
  body TEXT NOT NULL,

  -- JSON array of `{ "path": <absolute path>, "name": <display name> }`.
  -- Only attachments this machine can open again by path. A browser `File`
  -- (a drop, or a remote client's picker) and an Android `content://` grant
  -- cannot outlive the page that holds them, so the client never offers them
  -- here and the command refuses a path that is not absolute.
  attachments TEXT NOT NULL,

  -- JSON array of conversation ids dragged in as references. Titles are read
  -- at load time rather than copied, since a rename would otherwise leave the
  -- chip saying something the sidebar no longer does.
  conversation_refs TEXT NOT NULL,

  -- A sticker waiting to go with the text. Deleting the sticker takes it off
  -- the draft rather than blocking the delete.
  sticker_id TEXT REFERENCES emojis(id) ON DELETE SET NULL,

  -- Monotonic per slot, chosen by the writer. A write is applied only when its
  -- revision is higher than the stored one, so a save that set off first and
  -- landed last — two async invokes are not ordered by the runtime — cannot
  -- put back text that has since been changed.
  revision BIGINT NOT NULL CHECK (revision > 0),

  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,

  CHECK (
    (slot = 'new' AND conversation_id IS NULL)
    OR (conversation_id IS NOT NULL AND slot = 'conversation:' || conversation_id)
  )
);
