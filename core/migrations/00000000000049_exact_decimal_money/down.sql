CREATE TABLE model_configs_real (
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
    capability_overrides TEXT,
    cache_write_price REAL,
    price_tiers TEXT,
    server_tools TEXT,
    server_tool_price REAL,
    UNIQUE(provider_id, model_id)
);

INSERT INTO model_configs_real
SELECT id, provider_id, model_id, display_name, context_window,
       compact_threshold, max_output_tokens,
       COALESCE(CAST(input_price AS REAL), 0),
       COALESCE(CAST(output_price AS REAL), 0),
       CAST(cache_read_price AS REAL), created_at, updated_at, capability_overrides,
       CAST(cache_write_price AS REAL),
       CASE WHEN pricing_tiers IS NULL THEN NULL ELSE (
           SELECT json_group_array(json(CASE
               WHEN tier.type <> 'object'
                 OR json_type(tier.value, '$.input_price') IS NULL
                 OR json_type(tier.value, '$.input_price') <> 'text'
                 OR json_type(tier.value, '$.output_price') IS NULL
                 OR json_type(tier.value, '$.output_price') <> 'text'
                 OR (json_type(tier.value, '$.cache_read_price') IS NOT NULL
                     AND json_type(tier.value, '$.cache_read_price') NOT IN ('null', 'text'))
                 OR (json_type(tier.value, '$.cache_write_price') IS NOT NULL
                     AND json_type(tier.value, '$.cache_write_price') NOT IN ('null', 'text'))
               THEN tier.value
               ELSE json_remove(
                   json_patch(
                       tier.value,
                       json_object(
                           'input', CAST(json_extract(tier.value, '$.input_price') AS REAL),
                           'output', CAST(json_extract(tier.value, '$.output_price') AS REAL),
                           'cache_read', CASE
                               WHEN json_type(tier.value, '$.cache_read_price') IS NULL
                                 OR json_type(tier.value, '$.cache_read_price') = 'null' THEN NULL
                               ELSE CAST(json_extract(tier.value, '$.cache_read_price') AS REAL)
                           END,
                           'cache_write', CASE
                               WHEN json_type(tier.value, '$.cache_write_price') IS NULL
                                 OR json_type(tier.value, '$.cache_write_price') = 'null' THEN NULL
                               ELSE CAST(json_extract(tier.value, '$.cache_write_price') AS REAL)
                           END
                       )
                   ),
                   '$.input_price', '$.output_price', '$.cache_read_price', '$.cache_write_price'
               )
           END)) FROM json_each(model_configs.pricing_tiers) AS tier
       ) END,
       server_tools, CAST(server_tool_price AS REAL)
FROM model_configs;

DROP TABLE model_configs;
ALTER TABLE model_configs_real RENAME TO model_configs;

CREATE TABLE audit_messages_real (
    id TEXT PRIMARY KEY NOT NULL,
    recorded_at BIGINT NOT NULL,
    message_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    turn_id TEXT,
    source_type TEXT,
    source_id TEXT,
    turn_origin TEXT,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    sender_id BIGINT,
    sender_name TEXT,
    provider_id TEXT,
    provider_name TEXT,
    model_id TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    created_at BIGINT NOT NULL,
    input_price REAL,
    output_price REAL,
    cache_read_price REAL,
    cache_write_price REAL,
    self_id BIGINT,
    server_tool_calls INTEGER,
    server_tool_price REAL,
    billing_mode TEXT NOT NULL DEFAULT 'metered'
);

INSERT INTO audit_messages_real
SELECT id, recorded_at, message_id, conversation_id, turn_id, source_type,
       source_id, turn_origin, role, content, sender_id, sender_name, provider_id,
       provider_name, model_id, input_tokens, output_tokens, cache_read_tokens,
       cache_write_tokens, created_at, CAST(input_price AS REAL),
       CAST(output_price AS REAL), CAST(cache_read_price AS REAL),
       CAST(cache_write_price AS REAL), self_id, server_tool_calls,
       CAST(server_tool_price AS REAL), billing_mode
FROM audit_messages;

DROP TABLE audit_messages;
ALTER TABLE audit_messages_real RENAME TO audit_messages;

CREATE INDEX idx_audit_created ON audit_messages(created_at);
CREATE INDEX idx_audit_sender ON audit_messages(sender_id) WHERE sender_id IS NOT NULL;
CREATE INDEX idx_audit_provider_model ON audit_messages(provider_id, model_id);
CREATE INDEX idx_audit_conversation_turn ON audit_messages(conversation_id, turn_id);
