-- A model is one thing; reaching it through a provider is another. The same
-- Claude answers on Anthropic, Vertex and Azure, and until now each of those
-- carried its own context window, its own capability patch and its own prices —
-- configured three times, drifting apart afterwards. `model_profiles` holds
-- what the model *is*; `model_configs` keeps what one provider calls it and,
-- when the rates really do differ, what that provider charges.
--
-- The money CHECKs are migration 49's, column for column: a price is canonical
-- fixed-point TEXT or it is not stored.

CREATE TABLE model_profiles (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
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
    capability_overrides TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

-- One profile per existing row, keeping that row's id so nothing has to be
-- matched up by name. Models shared across providers are merged by the user
-- afterwards, which is a decision only they can make: two rows naming
-- `claude-sonnet-5` may genuinely be two deployments with two context windows.
INSERT INTO model_profiles (
    id, name, context_window, compact_threshold, max_output_tokens,
    input_price, output_price, cache_read_price, cache_write_price,
    pricing_tiers, capability_overrides, created_at, updated_at
)
SELECT
    id, COALESCE(NULLIF(display_name, ''), model_id), context_window, compact_threshold, max_output_tokens,
    input_price, output_price, cache_read_price, cache_write_price,
    pricing_tiers, capability_overrides, created_at, updated_at
FROM model_configs;

CREATE TABLE model_configs_profiled (
    id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id TEXT NOT NULL,
    -- No ON DELETE: a profile with models pointing at it may not be deleted,
    -- and a config without a profile has no context window to run a turn with.
    profile_id TEXT NOT NULL REFERENCES model_profiles(id),
    -- Off means the four price columns and the tiers below are not read at all.
    -- Blank-and-ignored and blank-and-meaningful are different states, so the
    -- switch says which one it is rather than leaving it to be guessed.
    overrides_pricing INTEGER NOT NULL DEFAULT 0 CHECK (overrides_pricing IN (0, 1)),
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
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE(provider_id, model_id)
);

-- Every existing row starts unoverridden: its prices went to the profile it
-- just created, and copying them here as well would make the same number true
-- in two places, which is how they come to disagree.
INSERT INTO model_configs_profiled (
    id, provider_id, model_id, profile_id, overrides_pricing,
    input_price, output_price, cache_read_price, cache_write_price, pricing_tiers,
    server_tools, server_tool_price, created_at, updated_at
)
SELECT
    id, provider_id, model_id, id, 0,
    NULL, NULL, NULL, NULL, NULL,
    server_tools, server_tool_price, created_at, updated_at
FROM model_configs;

DROP TABLE model_configs;
ALTER TABLE model_configs_profiled RENAME TO model_configs;

-- What `delete_if_unreferenced` and the profile list's model count both ask.
CREATE INDEX idx_model_configs_profile ON model_configs(profile_id);
