ALTER TABLE emoji_packs ADD COLUMN kind TEXT NOT NULL DEFAULT 'manual';
ALTER TABLE emoji_packs ADD COLUMN source_account_id TEXT;

ALTER TABLE emojis ADD COLUMN source TEXT NOT NULL DEFAULT 'local';
ALTER TABLE emojis ADD COLUMN source_key TEXT;
ALTER TABLE emojis ADD COLUMN native_payload TEXT;
ALTER TABLE emojis ADD COLUMN semantic_status TEXT NOT NULL DEFAULT 'confirmed';
ALTER TABLE emojis ADD COLUMN suggested_name TEXT;
ALTER TABLE emojis ADD COLUMN suggested_tags TEXT;
ALTER TABLE emojis ADD COLUMN file_size BIGINT NOT NULL DEFAULT 0;
ALTER TABLE emojis ADD COLUMN seen_count INTEGER NOT NULL DEFAULT 1;
ALTER TABLE emojis ADD COLUMN last_seen_at BIGINT;

UPDATE emojis SET last_seen_at = created_at WHERE last_seen_at IS NULL;

CREATE UNIQUE INDEX idx_emoji_packs_source_account
  ON emoji_packs(kind, source_account_id)
  WHERE source_account_id IS NOT NULL;
CREATE UNIQUE INDEX idx_emojis_source_key
  ON emojis(pack_id, source, source_key)
  WHERE source_key IS NOT NULL;
CREATE INDEX idx_emojis_semantic_status
  ON emojis(pack_id, semantic_status, last_seen_at);

CREATE TABLE message_stickers (
  message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  sticker_id TEXT NOT NULL REFERENCES emojis(id) ON DELETE RESTRICT,
  position INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (message_id, position)
);
CREATE INDEX idx_message_stickers_sticker ON message_stickers(sticker_id);
