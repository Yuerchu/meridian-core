-- Opaque continuation state returned by an upstream model.
--
-- This is deliberately separate from visible reasoning and semantic tool
-- calls. Providers sign different wire objects, and some attach more than one
-- token to a single assistant message. The value is a versioned JSON storage
-- DTO owned by the provider layer; it is never part of the frontend message
-- DTO or a conversation export.
ALTER TABLE messages ADD COLUMN provider_state TEXT;
