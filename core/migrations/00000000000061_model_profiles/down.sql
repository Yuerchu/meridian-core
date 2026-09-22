-- Folds each profile back into every config that pointed at it. A profile
-- shared by three providers becomes three identical copies again, which is
-- exactly the state this migration was written to end.

CREATE TABLE model_configs_flat (
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

INSERT INTO model_configs_flat (
    id, provider_id, model_id, display_name, context_window,
    compact_threshold, max_output_tokens, input_price, output_price,
    cache_read_price, created_at, updated_at, capability_overrides,
    cache_write_price, pricing_tiers, server_tools, server_tool_price
)
SELECT
    c.id, c.provider_id, c.model_id, p.name, p.context_window,
    p.compact_threshold, p.max_output_tokens,
    CASE WHEN c.overrides_pricing = 1 THEN c.input_price ELSE p.input_price END,
    CASE WHEN c.overrides_pricing = 1 THEN c.output_price ELSE p.output_price END,
    CASE WHEN c.overrides_pricing = 1 THEN c.cache_read_price ELSE p.cache_read_price END,
    c.created_at, c.updated_at, p.capability_overrides,
    CASE WHEN c.overrides_pricing = 1 THEN c.cache_write_price ELSE p.cache_write_price END,
    CASE WHEN c.overrides_pricing = 1 THEN c.pricing_tiers ELSE p.pricing_tiers END,
    c.server_tools, c.server_tool_price
FROM model_configs AS c
JOIN model_profiles AS p ON p.id = c.profile_id;

DROP TABLE model_configs;
ALTER TABLE model_configs_flat RENAME TO model_configs;
DROP TABLE model_profiles;
