-- An endpoint that is nobody's published product.
--
-- The five formats so far are either our own contract (`generic`) or a vendor's
-- (`dingtalk`, `feishu`, `wecom`, `slack`). What neither covers is the common
-- case of a company's own alert pipe: it has a fixed schema somebody's
-- operations team wrote down, and it authenticates with a bearer token.
--
-- `custom` is that: the body comes from `body_template` and the stored secret
-- is the bearer token rather than a signing key. **What the secret means has
-- always been decided by the format** — DingTalk signs a URL with it, Feishu
-- signs a body, `generic` computes an HMAC header — so this is one more entry
-- in that table rather than a new concept, and it needs no second keyring slot.
--
-- The CHECK constraint has to be rebuilt to admit the new value; SQLite cannot
-- alter one in place. No other table references this one, so the rename below
-- rewrites nobody's REFERENCES clause.

CREATE TABLE notification_webhooks_new (
  id                   TEXT PRIMARY KEY NOT NULL,
  name                 TEXT NOT NULL,
  url                  TEXT NOT NULL,
  format               TEXT NOT NULL CHECK (format IN ('generic', 'dingtalk', 'feishu', 'wecom', 'slack', 'custom')),
  events               TEXT NOT NULL,
  is_enabled           INTEGER NOT NULL DEFAULT 1 CHECK (is_enabled IN (0, 1)),
  -- The JSON document to POST, with placeholders, for `custom` only.
  --
  -- NULL for every other format, and required for `custom`: a custom endpoint
  -- with no body would post nothing and be reported as delivered. The check is
  -- in Rust rather than here because the message has somewhere useful to point.
  --
  -- TEXT is storage, not a public string contract — the same rule `events`
  -- follows. Requests and responses carry the decoded object, and a template
  -- that will not parse fails the boundary instead of silently posting `{}`.
  body_template        TEXT,
  last_attempt_at      BIGINT,
  last_success_at      BIGINT,
  last_error           TEXT,
  consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
  created_at           BIGINT NOT NULL,
  updated_at           BIGINT NOT NULL
);

INSERT INTO notification_webhooks_new
  (id, name, url, format, events, is_enabled, body_template,
   last_attempt_at, last_success_at, last_error, consecutive_failures, created_at, updated_at)
SELECT
   id, name, url, format, events, is_enabled, NULL,
   last_attempt_at, last_success_at, last_error, consecutive_failures, created_at, updated_at
  FROM notification_webhooks;

DROP TABLE notification_webhooks;
ALTER TABLE notification_webhooks_new RENAME TO notification_webhooks;
CREATE INDEX idx_notification_webhooks_enabled ON notification_webhooks(is_enabled);
