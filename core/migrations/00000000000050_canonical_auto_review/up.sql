-- `messages.auto_review` predates the public AutoReviewVerdict contract.  Its
-- legacy value duplicated usage already recorded in `audit_messages` and
-- omitted absent nullable keys (and an empty evidence list).  Rewrite every
-- structurally valid legacy verdict once so runtime readers have exactly one
-- shape to accept: required nullable keys use JSON null and evidence is always
-- an array.  The guard table deliberately turns malformed or ambiguous JSON
-- into a migration error instead of silently replacing it with an empty map.

CREATE TEMP TABLE migration_50_auto_review_guard (
    ok INTEGER NOT NULL CONSTRAINT valid_auto_review_shape CHECK (ok = 1)
);

-- Validate JSON text before any JSON function is allowed to inspect it.
INSERT INTO migration_50_auto_review_guard (ok)
SELECT CASE WHEN json_valid(auto_review) THEN 1 ELSE 0 END
FROM messages
WHERE auto_review IS NOT NULL;

INSERT INTO migration_50_auto_review_guard (ok)
SELECT CASE WHEN json_type(auto_review) = 'object' THEN 1 ELSE 0 END
FROM messages
WHERE auto_review IS NOT NULL;

-- The outer object is keyed by a non-empty call id and each value is one
-- verdict.  Duplicate keys are ambiguous and therefore rejected.
INSERT INTO migration_50_auto_review_guard (ok)
SELECT CASE
           WHEN verdict.type = 'object' AND verdict.key <> '' THEN 1
           ELSE 0
       END
FROM messages
JOIN json_each(messages.auto_review) AS verdict
WHERE messages.auto_review IS NOT NULL;

INSERT INTO migration_50_auto_review_guard (ok)
SELECT CASE WHEN COUNT(*) = COUNT(DISTINCT verdict.key) THEN 1 ELSE 0 END
FROM messages
JOIN json_each(messages.auto_review) AS verdict
WHERE messages.auto_review IS NOT NULL
GROUP BY messages.id;

-- Accept the one legacy-only `usage` member solely inside this migration.  All
-- other members are the canonical verdict names; unknown and duplicate names
-- make the old value impossible to interpret without guessing.
INSERT INTO migration_50_auto_review_guard (ok)
SELECT CASE
           WHEN EXISTS (
               SELECT 1
               FROM json_each(verdict.value) AS field
               WHERE field.key NOT IN (
                   'outcome', 'risk', 'authorization', 'rationale',
                   'stage', 'model', 'evidence', 'usage'
               )
           ) THEN 0
           WHEN (
               SELECT COUNT(*) FROM json_each(verdict.value)
           ) <> (
               SELECT COUNT(DISTINCT field.key) FROM json_each(verdict.value) AS field
           ) THEN 0
           WHEN json_type(verdict.value, '$.outcome') IS NOT 'text' THEN 0
           WHEN json_extract(verdict.value, '$.outcome') NOT IN ('allow', 'deny', 'unreadable') THEN 0
           WHEN json_type(verdict.value, '$.risk') IS NOT NULL
                AND json_type(verdict.value, '$.risk') NOT IN ('null', 'text') THEN 0
           WHEN json_type(verdict.value, '$.risk') = 'text'
                AND json_extract(verdict.value, '$.risk') NOT IN ('low', 'medium', 'high', 'critical') THEN 0
           WHEN json_type(verdict.value, '$.authorization') IS NOT NULL
                AND json_type(verdict.value, '$.authorization') NOT IN ('null', 'text') THEN 0
           WHEN json_type(verdict.value, '$.authorization') = 'text'
                AND json_extract(verdict.value, '$.authorization') NOT IN ('unknown', 'low', 'medium', 'high') THEN 0
           WHEN json_type(verdict.value, '$.rationale') IS NOT NULL
                AND json_type(verdict.value, '$.rationale') NOT IN ('null', 'text') THEN 0
           WHEN json_type(verdict.value, '$.stage') IS NOT NULL
                AND json_type(verdict.value, '$.stage') NOT IN ('null', 'text') THEN 0
           WHEN json_type(verdict.value, '$.stage') = 'text'
                AND json_extract(verdict.value, '$.stage') NOT IN ('quick', 'investigate') THEN 0
           WHEN json_type(verdict.value, '$.model') IS NOT NULL
                AND json_type(verdict.value, '$.model') NOT IN ('null', 'text') THEN 0
           WHEN json_type(verdict.value, '$.evidence') IS NOT NULL
                AND json_type(verdict.value, '$.evidence') <> 'array' THEN 0
           WHEN json_type(verdict.value, '$.usage') IS NOT NULL
                AND json_type(verdict.value, '$.usage') <> 'object' THEN 0
           WHEN json_type(verdict.value, '$.usage') = 'object'
                AND (
                    SELECT COUNT(*) FROM json_each(verdict.value, '$.usage')
                ) <> 4 THEN 0
           WHEN json_type(verdict.value, '$.usage') = 'object'
                AND (
                    SELECT COUNT(DISTINCT usage_field.key)
                    FROM json_each(verdict.value, '$.usage') AS usage_field
                ) <> 4 THEN 0
           WHEN json_type(verdict.value, '$.usage') = 'object'
                AND EXISTS (
                    SELECT 1
                    FROM json_each(verdict.value, '$.usage') AS usage_field
                    WHERE usage_field.key NOT IN (
                        'input_tokens', 'output_tokens',
                        'cache_read_tokens', 'cache_write_tokens'
                    )
                       OR usage_field.type NOT IN ('integer', 'null')
                       OR (usage_field.type = 'integer' AND usage_field.value < 0)
                ) THEN 0
           ELSE 1
       END
FROM messages
JOIN json_each(messages.auto_review) AS verdict
WHERE messages.auto_review IS NOT NULL
  AND verdict.type = 'object';

-- Evidence is first-party structured data as well: every array member has the
-- exact two string fields understood by the current DTO.
INSERT INTO migration_50_auto_review_guard (ok)
SELECT CASE
           WHEN evidence.type <> 'object' THEN 0
           WHEN json_type(evidence.value, '$.tool') IS NOT 'text' THEN 0
           WHEN json_type(evidence.value, '$.arguments') IS NOT 'text' THEN 0
           WHEN (
               SELECT COUNT(*) FROM json_each(evidence.value)
           ) <> 2 THEN 0
           WHEN (
               SELECT COUNT(DISTINCT field.key) FROM json_each(evidence.value) AS field
           ) <> 2 THEN 0
           WHEN EXISTS (
               SELECT 1
               FROM json_each(evidence.value) AS field
               WHERE field.key NOT IN ('tool', 'arguments')
           ) THEN 0
           ELSE 1
       END
FROM messages
JOIN json_each(messages.auto_review) AS verdict
JOIN json_each(verdict.value, '$.evidence') AS evidence
WHERE messages.auto_review IS NOT NULL
  AND verdict.type = 'object'
  AND json_type(verdict.value, '$.evidence') = 'array';

UPDATE messages
SET auto_review = (
    SELECT json_group_object(
        verdict.key,
        json_object(
            'outcome', json_extract(verdict.value, '$.outcome'),
            'risk', json_extract(verdict.value, '$.risk'),
            'authorization', json_extract(verdict.value, '$.authorization'),
            'rationale', json_extract(verdict.value, '$.rationale'),
            'stage', json_extract(verdict.value, '$.stage'),
            'model', json_extract(verdict.value, '$.model'),
            'evidence', json(
                CASE
                    WHEN json_type(verdict.value, '$.evidence') = 'array'
                    THEN json_extract(verdict.value, '$.evidence')
                    ELSE '[]'
                END
            )
        )
    )
    FROM json_each(messages.auto_review) AS verdict
)
WHERE auto_review IS NOT NULL;

DROP TABLE migration_50_auto_review_guard;
