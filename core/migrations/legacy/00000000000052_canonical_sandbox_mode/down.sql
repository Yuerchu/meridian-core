-- `container` has no lossless representation in the old boolean setting.
-- Refuse that downgrade instead of silently weakening the user's sandbox.
CREATE TEMP TABLE migration_52_sandbox_down_guard (
    ok INTEGER NOT NULL CONSTRAINT representable_sandbox_mode CHECK (ok = 1)
);

INSERT INTO migration_52_sandbox_down_guard (ok)
SELECT CASE WHEN value = 'container' THEN 0 ELSE 1 END
FROM preferences
WHERE key = 'sandbox.enabled';

UPDATE preferences
SET value = CASE value
    WHEN 'auto' THEN 'true'
    WHEN 'off' THEN 'false'
END
WHERE key = 'sandbox.enabled'
  AND value IN ('auto', 'off');

DROP TABLE migration_52_sandbox_down_guard;
