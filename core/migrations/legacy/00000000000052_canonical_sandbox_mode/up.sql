-- `sandbox.enabled` used to be a boolean preference.  The execution-mode
-- selector keeps the same key but now stores one of `auto`, `off`, or
-- `container`.  Preserve the user's old choice when upgrading: enabled meant
-- the best sandbox available on this platform (`auto`), while disabled is
-- exactly `off`.
UPDATE preferences
SET value = CASE value
    WHEN 'true' THEN 'auto'
    WHEN 'false' THEN 'off'
END
WHERE key = 'sandbox.enabled'
  AND value IN ('true', 'false');
