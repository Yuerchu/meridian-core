CREATE TABLE emoji_packs (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  description TEXT,
  cover_image TEXT,
  is_builtin INTEGER NOT NULL DEFAULT 0,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE TABLE emojis (
  id TEXT PRIMARY KEY NOT NULL,
  pack_id TEXT NOT NULL REFERENCES emoji_packs(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  tags TEXT,
  file_name TEXT NOT NULL,
  file_format TEXT NOT NULL DEFAULT 'gif',
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at BIGINT NOT NULL
);

CREATE INDEX idx_emojis_pack ON emojis(pack_id, sort_order);
CREATE UNIQUE INDEX idx_emojis_pack_name ON emojis(pack_id, name);

CREATE TABLE assistant_emoji_packs (
  assistant_id TEXT NOT NULL REFERENCES assistants(id) ON DELETE CASCADE,
  pack_id TEXT NOT NULL REFERENCES emoji_packs(id) ON DELETE CASCADE,
  created_at BIGINT NOT NULL,
  PRIMARY KEY (assistant_id, pack_id)
);
