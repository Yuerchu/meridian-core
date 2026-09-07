ALTER TABLE conversations ADD COLUMN compact_cursor INTEGER;
ALTER TABLE messages ADD COLUMN is_compact_summary INTEGER NOT NULL DEFAULT 0;
ALTER TABLE assistants ADD COLUMN auto_compact_enabled INTEGER NOT NULL DEFAULT 0;
