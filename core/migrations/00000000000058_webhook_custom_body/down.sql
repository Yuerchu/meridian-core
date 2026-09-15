-- Rows whose format is `custom` cannot survive the narrower constraint, so they
-- go with it rather than failing the whole migration.
DELETE FROM notification_webhooks WHERE format = 'custom';

CREATE TABLE notification_webhooks_old (
  id                   TEXT PRIMARY KEY NOT NULL,
  name                 TEXT NOT NULL,
  url                  TEXT NOT NULL,
  format               TEXT NOT NULL CHECK (format IN ('generic', 'dingtalk', 'feishu', 'wecom', 'slack')),
  events               TEXT NOT NULL,
  is_enabled           INTEGER NOT NULL DEFAULT 1 CHECK (is_enabled IN (0, 1)),
  last_attempt_at      BIGINT,
  last_success_at      BIGINT,
  last_error           TEXT,
  consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
  created_at           BIGINT NOT NULL,
  updated_at           BIGINT NOT NULL
);

INSERT INTO notification_webhooks_old
  (id, name, url, format, events, is_enabled,
   last_attempt_at, last_success_at, last_error, consecutive_failures, created_at, updated_at)
SELECT
   id, name, url, format, events, is_enabled,
   last_attempt_at, last_success_at, last_error, consecutive_failures, created_at, updated_at
  FROM notification_webhooks;

DROP TABLE notification_webhooks;
ALTER TABLE notification_webhooks_old RENAME TO notification_webhooks;
CREATE INDEX idx_notification_webhooks_enabled ON notification_webhooks(is_enabled);
