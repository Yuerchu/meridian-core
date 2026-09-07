-- These two tables never acquired a runtime reader or writer. Attachments are
-- stored as OpenAI-style parts in messages.content, and tool permissions are
-- enforced by the tool reach/approval policy instead.
DROP TABLE attachments;
DROP TABLE tool_permissions;

-- Migration 21 copied every legacy sort-order boundary to the branch-aware
-- messages.compact_anchor_id. Nothing has read or written this cursor since.
ALTER TABLE conversations DROP COLUMN compact_cursor;
