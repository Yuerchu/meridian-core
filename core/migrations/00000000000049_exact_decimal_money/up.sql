-- Money is never stored through SQLite's REAL/NUMERIC affinity: both can turn
-- an exact decimal into an IEEE-754 value. Canonical fixed-point TEXT keeps the
-- NUMERIC(38,18) contract intact and is what the Rust/IPC Decimal type reads.

CREATE TABLE model_configs_decimal (
    id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id TEXT NOT NULL,
    display_name TEXT,
    context_window INTEGER NOT NULL,
    compact_threshold INTEGER NOT NULL,
    max_output_tokens INTEGER,
    input_price TEXT CHECK (
        input_price IS NULL OR input_price = '0' OR (
            typeof(input_price) = 'text'
            AND length(input_price) > 0
            AND input_price NOT GLOB '*[^0-9.]*'
            AND input_price NOT LIKE '%.%.%'
            AND (substr(input_price, 1, 1) <> '0' OR substr(input_price, 2, 1) = '.')
            AND (instr(input_price, '.') = 0 OR (
                instr(input_price, '.') BETWEEN 2 AND length(input_price) - 1
                AND substr(input_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(input_price, '.') = 0 THEN input_price
                            ELSE substr(input_price, 1, instr(input_price, '.') - 1) END) <= 20
            AND (instr(input_price, '.') = 0
                 OR length(input_price) - instr(input_price, '.') <= 18)
        )
    ),
    output_price TEXT CHECK (
        output_price IS NULL OR output_price = '0' OR (
            typeof(output_price) = 'text'
            AND length(output_price) > 0
            AND output_price NOT GLOB '*[^0-9.]*'
            AND output_price NOT LIKE '%.%.%'
            AND (substr(output_price, 1, 1) <> '0' OR substr(output_price, 2, 1) = '.')
            AND (instr(output_price, '.') = 0 OR (
                instr(output_price, '.') BETWEEN 2 AND length(output_price) - 1
                AND substr(output_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(output_price, '.') = 0 THEN output_price
                            ELSE substr(output_price, 1, instr(output_price, '.') - 1) END) <= 20
            AND (instr(output_price, '.') = 0
                 OR length(output_price) - instr(output_price, '.') <= 18)
        )
    ),
    cache_read_price TEXT CHECK (
        cache_read_price IS NULL OR cache_read_price = '0' OR (
            typeof(cache_read_price) = 'text'
            AND length(cache_read_price) > 0
            AND cache_read_price NOT GLOB '*[^0-9.]*'
            AND cache_read_price NOT LIKE '%.%.%'
            AND (substr(cache_read_price, 1, 1) <> '0' OR substr(cache_read_price, 2, 1) = '.')
            AND (instr(cache_read_price, '.') = 0 OR (
                instr(cache_read_price, '.') BETWEEN 2 AND length(cache_read_price) - 1
                AND substr(cache_read_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(cache_read_price, '.') = 0 THEN cache_read_price
                            ELSE substr(cache_read_price, 1, instr(cache_read_price, '.') - 1) END) <= 20
            AND (instr(cache_read_price, '.') = 0
                 OR length(cache_read_price) - instr(cache_read_price, '.') <= 18)
        )
    ),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    capability_overrides TEXT,
    cache_write_price TEXT CHECK (
        cache_write_price IS NULL OR cache_write_price = '0' OR (
            typeof(cache_write_price) = 'text'
            AND length(cache_write_price) > 0
            AND cache_write_price NOT GLOB '*[^0-9.]*'
            AND cache_write_price NOT LIKE '%.%.%'
            AND (substr(cache_write_price, 1, 1) <> '0' OR substr(cache_write_price, 2, 1) = '.')
            AND (instr(cache_write_price, '.') = 0 OR (
                instr(cache_write_price, '.') BETWEEN 2 AND length(cache_write_price) - 1
                AND substr(cache_write_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(cache_write_price, '.') = 0 THEN cache_write_price
                            ELSE substr(cache_write_price, 1, instr(cache_write_price, '.') - 1) END) <= 20
            AND (instr(cache_write_price, '.') = 0
                 OR length(cache_write_price) - instr(cache_write_price, '.') <= 18)
        )
    ),
    pricing_tiers TEXT CHECK (
        pricing_tiers IS NULL OR (json_valid(pricing_tiers) AND json_type(pricing_tiers) = 'array')
    ),
    server_tools TEXT,
    server_tool_price TEXT CHECK (
        server_tool_price IS NULL OR server_tool_price = '0' OR (
            typeof(server_tool_price) = 'text'
            AND length(server_tool_price) > 0
            AND server_tool_price NOT GLOB '*[^0-9.]*'
            AND server_tool_price NOT LIKE '%.%.%'
            AND (substr(server_tool_price, 1, 1) <> '0' OR substr(server_tool_price, 2, 1) = '.')
            AND (instr(server_tool_price, '.') = 0 OR (
                instr(server_tool_price, '.') BETWEEN 2 AND length(server_tool_price) - 1
                AND substr(server_tool_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(server_tool_price, '.') = 0 THEN server_tool_price
                            ELSE substr(server_tool_price, 1, instr(server_tool_price, '.') - 1) END) <= 20
            AND (instr(server_tool_price, '.') = 0
                 OR length(server_tool_price) - instr(server_tool_price, '.') <= 18)
        )
    ),
    UNIQUE(provider_id, model_id)
);

INSERT INTO model_configs_decimal (
    id, provider_id, model_id, display_name, context_window,
    compact_threshold, max_output_tokens, input_price, output_price,
    cache_read_price, created_at, updated_at, capability_overrides,
    cache_write_price, pricing_tiers, server_tools, server_tool_price
)
SELECT
    id, provider_id, model_id, display_name, context_window,
    compact_threshold, max_output_tokens,
    CASE WHEN typeof(input_price) IN ('integer', 'real')
              AND typeof(output_price) IN ('integer', 'real')
              AND input_price = 0 AND output_price = 0 THEN NULL
         WHEN typeof(input_price) IN ('integer', 'real')
         THEN rtrim(rtrim(printf('%.18f', input_price), '0'), '.')
         ELSE input_price END,
    CASE WHEN typeof(input_price) IN ('integer', 'real')
              AND typeof(output_price) IN ('integer', 'real')
              AND input_price = 0 AND output_price = 0 THEN NULL
         WHEN typeof(output_price) IN ('integer', 'real')
         THEN rtrim(rtrim(printf('%.18f', output_price), '0'), '.')
         ELSE output_price END,
    CASE WHEN cache_price IS NULL THEN NULL
         WHEN typeof(cache_price) IN ('integer', 'real')
         THEN rtrim(rtrim(printf('%.18f', cache_price), '0'), '.')
         ELSE cache_price END,
    created_at, updated_at, capability_overrides,
    CASE WHEN cache_write_price IS NULL THEN NULL
         WHEN typeof(cache_write_price) IN ('integer', 'real')
         THEN rtrim(rtrim(printf('%.18f', cache_write_price), '0'), '.')
         ELSE cache_write_price END,
    CASE
        WHEN price_tiers IS NULL THEN NULL
        WHEN json_type(price_tiers) <> 'array' THEN price_tiers
        ELSE (
            SELECT json_group_array(json(patched))
            FROM (
                SELECT CASE
                    WHEN tier.type <> 'object'
                      OR json_type(tier.value, '$.min_prompt_tokens') <> 'integer'
                      OR json_type(tier.value, '$.input') IS NULL
                      OR json_type(tier.value, '$.input') NOT IN ('integer', 'real')
                      OR json_type(tier.value, '$.output') IS NULL
                      OR json_type(tier.value, '$.output') NOT IN ('integer', 'real')
                      OR (json_type(tier.value, '$.cache_read') IS NOT NULL
                          AND json_type(tier.value, '$.cache_read') NOT IN ('null', 'integer', 'real'))
                      OR (json_type(tier.value, '$.cache_write') IS NOT NULL
                          AND json_type(tier.value, '$.cache_write') NOT IN ('null', 'integer', 'real'))
                    THEN tier.value
                    ELSE json_remove(
                        json_set(
                            tier.value,
                            '$.input_price', rtrim(rtrim(printf('%.18f', json_extract(tier.value, '$.input')), '0'), '.'),
                            '$.output_price', rtrim(rtrim(printf('%.18f', json_extract(tier.value, '$.output')), '0'), '.'),
                            '$.cache_read_price', CASE
                                WHEN json_type(tier.value, '$.cache_read') IS NULL
                                  OR json_type(tier.value, '$.cache_read') = 'null' THEN NULL
                                ELSE rtrim(rtrim(printf('%.18f', json_extract(tier.value, '$.cache_read')), '0'), '.')
                            END,
                            '$.cache_write_price', CASE
                                WHEN json_type(tier.value, '$.cache_write') IS NULL
                                  OR json_type(tier.value, '$.cache_write') = 'null' THEN NULL
                                ELSE rtrim(rtrim(printf('%.18f', json_extract(tier.value, '$.cache_write')), '0'), '.')
                            END
                        ),
                        '$.input', '$.output', '$.cache_read', '$.cache_write'
                    )
                END AS patched
                FROM json_each(model_configs.price_tiers) AS tier
                ORDER BY CAST(tier.key AS INTEGER)
            )
        )
    END,
    server_tools,
    CASE WHEN server_tool_price IS NULL THEN NULL
         WHEN typeof(server_tool_price) IN ('integer', 'real')
         THEN rtrim(rtrim(printf('%.18f', server_tool_price), '0'), '.')
         ELSE server_tool_price END
FROM model_configs;

DROP TABLE model_configs;
ALTER TABLE model_configs_decimal RENAME TO model_configs;

CREATE TABLE audit_messages_decimal (
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
    input_price TEXT CHECK (input_price IS NULL OR input_price = '0' OR (
        typeof(input_price) = 'text'
        AND length(input_price) > 0 AND input_price NOT GLOB '*[^0-9.]*' AND input_price NOT LIKE '%.%.%'
        AND (substr(input_price, 1, 1) <> '0' OR substr(input_price, 2, 1) = '.')
        AND (instr(input_price, '.') = 0 OR (instr(input_price, '.') BETWEEN 2 AND length(input_price) - 1 AND substr(input_price, -1, 1) GLOB '[1-9]'))
        AND length(CASE WHEN instr(input_price, '.') = 0 THEN input_price ELSE substr(input_price, 1, instr(input_price, '.') - 1) END) <= 20
        AND (instr(input_price, '.') = 0 OR length(input_price) - instr(input_price, '.') <= 18)
    )),
    output_price TEXT CHECK (output_price IS NULL OR output_price = '0' OR (
        typeof(output_price) = 'text'
        AND length(output_price) > 0 AND output_price NOT GLOB '*[^0-9.]*' AND output_price NOT LIKE '%.%.%'
        AND (substr(output_price, 1, 1) <> '0' OR substr(output_price, 2, 1) = '.')
        AND (instr(output_price, '.') = 0 OR (instr(output_price, '.') BETWEEN 2 AND length(output_price) - 1 AND substr(output_price, -1, 1) GLOB '[1-9]'))
        AND length(CASE WHEN instr(output_price, '.') = 0 THEN output_price ELSE substr(output_price, 1, instr(output_price, '.') - 1) END) <= 20
        AND (instr(output_price, '.') = 0 OR length(output_price) - instr(output_price, '.') <= 18)
    )),
    cache_read_price TEXT CHECK (cache_read_price IS NULL OR cache_read_price = '0' OR (
        typeof(cache_read_price) = 'text'
        AND length(cache_read_price) > 0 AND cache_read_price NOT GLOB '*[^0-9.]*' AND cache_read_price NOT LIKE '%.%.%'
        AND (substr(cache_read_price, 1, 1) <> '0' OR substr(cache_read_price, 2, 1) = '.')
        AND (instr(cache_read_price, '.') = 0 OR (instr(cache_read_price, '.') BETWEEN 2 AND length(cache_read_price) - 1 AND substr(cache_read_price, -1, 1) GLOB '[1-9]'))
        AND length(CASE WHEN instr(cache_read_price, '.') = 0 THEN cache_read_price ELSE substr(cache_read_price, 1, instr(cache_read_price, '.') - 1) END) <= 20
        AND (instr(cache_read_price, '.') = 0 OR length(cache_read_price) - instr(cache_read_price, '.') <= 18)
    )),
    cache_write_price TEXT CHECK (cache_write_price IS NULL OR cache_write_price = '0' OR (
        typeof(cache_write_price) = 'text'
        AND length(cache_write_price) > 0 AND cache_write_price NOT GLOB '*[^0-9.]*' AND cache_write_price NOT LIKE '%.%.%'
        AND (substr(cache_write_price, 1, 1) <> '0' OR substr(cache_write_price, 2, 1) = '.')
        AND (instr(cache_write_price, '.') = 0 OR (instr(cache_write_price, '.') BETWEEN 2 AND length(cache_write_price) - 1 AND substr(cache_write_price, -1, 1) GLOB '[1-9]'))
        AND length(CASE WHEN instr(cache_write_price, '.') = 0 THEN cache_write_price ELSE substr(cache_write_price, 1, instr(cache_write_price, '.') - 1) END) <= 20
        AND (instr(cache_write_price, '.') = 0 OR length(cache_write_price) - instr(cache_write_price, '.') <= 18)
    )),
    self_id BIGINT,
    server_tool_calls INTEGER,
    server_tool_price TEXT CHECK (server_tool_price IS NULL OR server_tool_price = '0' OR (
        typeof(server_tool_price) = 'text'
        AND length(server_tool_price) > 0 AND server_tool_price NOT GLOB '*[^0-9.]*' AND server_tool_price NOT LIKE '%.%.%'
        AND (substr(server_tool_price, 1, 1) <> '0' OR substr(server_tool_price, 2, 1) = '.')
        AND (instr(server_tool_price, '.') = 0 OR (instr(server_tool_price, '.') BETWEEN 2 AND length(server_tool_price) - 1 AND substr(server_tool_price, -1, 1) GLOB '[1-9]'))
        AND length(CASE WHEN instr(server_tool_price, '.') = 0 THEN server_tool_price ELSE substr(server_tool_price, 1, instr(server_tool_price, '.') - 1) END) <= 20
        AND (instr(server_tool_price, '.') = 0 OR length(server_tool_price) - instr(server_tool_price, '.') <= 18)
    )),
    billing_mode TEXT NOT NULL DEFAULT 'metered'
        CHECK (billing_mode IN ('metered', 'subscription', 'external'))
);

INSERT INTO audit_messages_decimal
SELECT
    id, recorded_at, message_id, conversation_id, turn_id, source_type,
    source_id, turn_origin, role, content, sender_id, sender_name, provider_id,
    provider_name, model_id, input_tokens, output_tokens, cache_read_tokens,
    cache_write_tokens, created_at,
    CASE WHEN input_price IS NULL THEN NULL
         WHEN typeof(input_price) IN ('integer', 'real')
         THEN rtrim(rtrim(printf('%.18f', input_price), '0'), '.')
         ELSE input_price END,
    CASE WHEN output_price IS NULL THEN NULL
         WHEN typeof(output_price) IN ('integer', 'real')
         THEN rtrim(rtrim(printf('%.18f', output_price), '0'), '.')
         ELSE output_price END,
    CASE WHEN cache_read_price IS NULL THEN NULL
         WHEN typeof(cache_read_price) IN ('integer', 'real')
         THEN rtrim(rtrim(printf('%.18f', cache_read_price), '0'), '.')
         ELSE cache_read_price END,
    CASE WHEN cache_write_price IS NULL THEN NULL
         WHEN typeof(cache_write_price) IN ('integer', 'real')
         THEN rtrim(rtrim(printf('%.18f', cache_write_price), '0'), '.')
         ELSE cache_write_price END,
    self_id, server_tool_calls,
    CASE WHEN server_tool_price IS NULL THEN NULL
         WHEN typeof(server_tool_price) IN ('integer', 'real')
         THEN rtrim(rtrim(printf('%.18f', server_tool_price), '0'), '.')
         ELSE server_tool_price END,
    billing_mode
FROM audit_messages;

DROP TABLE audit_messages;
ALTER TABLE audit_messages_decimal RENAME TO audit_messages;

-- Optional money is represented by an absent preference, never an empty-string
-- sentinel that the Decimal parser would have to reinterpret.
DELETE FROM preferences
WHERE key = 'onebot.balance_alert_threshold' AND value = '';

CREATE INDEX idx_audit_created ON audit_messages(created_at);
CREATE INDEX idx_audit_sender ON audit_messages(sender_id) WHERE sender_id IS NOT NULL;
CREATE INDEX idx_audit_provider_model ON audit_messages(provider_id, model_id);
CREATE INDEX idx_audit_conversation_turn ON audit_messages(conversation_id, turn_id);
