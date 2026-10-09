-- The index goes first: SQLite refuses to drop a column an index refers to.
DROP INDEX idx_messages_parent;
ALTER TABLE messages DROP COLUMN parent_id;
ALTER TABLE messages DROP COLUMN compact_anchor_id;
ALTER TABLE conversations DROP COLUMN head_message_id;
