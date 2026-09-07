ALTER TABLE conversations DROP COLUMN compact_cursor;
ALTER TABLE messages DROP COLUMN is_compact_summary;
ALTER TABLE assistants DROP COLUMN auto_compact_enabled;
