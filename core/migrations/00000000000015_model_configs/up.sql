CREATE TABLE model_configs (
    id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id TEXT NOT NULL,
    display_name TEXT,
    context_window INTEGER NOT NULL,
    compact_threshold INTEGER NOT NULL,
    max_output_tokens INTEGER,
    input_price REAL NOT NULL DEFAULT 0,
    output_price REAL NOT NULL DEFAULT 0,
    cache_price REAL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE(provider_id, model_id)
);
