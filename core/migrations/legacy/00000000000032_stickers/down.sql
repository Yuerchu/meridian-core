DROP TABLE message_stickers;
DROP INDEX idx_emojis_semantic_status;
DROP INDEX idx_emojis_source_key;
DROP INDEX idx_emoji_packs_source_account;

ALTER TABLE emojis DROP COLUMN last_seen_at;
ALTER TABLE emojis DROP COLUMN seen_count;
ALTER TABLE emojis DROP COLUMN file_size;
ALTER TABLE emojis DROP COLUMN suggested_tags;
ALTER TABLE emojis DROP COLUMN suggested_name;
ALTER TABLE emojis DROP COLUMN semantic_status;
ALTER TABLE emojis DROP COLUMN native_payload;
ALTER TABLE emojis DROP COLUMN source_key;
ALTER TABLE emojis DROP COLUMN source;

ALTER TABLE emoji_packs DROP COLUMN source_account_id;
ALTER TABLE emoji_packs DROP COLUMN kind;
