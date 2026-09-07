ALTER TABLE assistants ADD COLUMN thinking_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE assistants ADD COLUMN thinking_budget INTEGER;
