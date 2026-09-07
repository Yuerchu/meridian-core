CREATE TABLE cached_models (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  model_id TEXT NOT NULL,
  model_name TEXT NOT NULL,
  fetched_at BIGINT NOT NULL
);

CREATE INDEX idx_cached_models_provider ON cached_models(provider_id);
CREATE UNIQUE INDEX idx_cached_models_unique ON cached_models(provider_id, model_id);
