ALTER TABLE model_configs ADD COLUMN capability_overrides TEXT;
ALTER TABLE conversations ADD COLUMN thinking_level TEXT;
ALTER TABLE conversations ADD COLUMN fast_mode INTEGER NOT NULL DEFAULT 0;
