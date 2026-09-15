-- Where an alert goes, and what has already been said.
--
-- Two tables because they answer two different questions and have different
-- lifetimes: an endpoint is configuration a person typed, and an alert state is
-- a fact about what this install has already reported.

-- One row per outbound endpoint.
--
-- The signing secret is deliberately NOT here. Every row of this table crosses
-- IPC as a response contract, so a secret column is a secret handed back on
-- every list. It lives in the keyring under NOTIFY_WEBHOOK_SECRET_<id>, the
-- same arrangement provider API keys already use, and the response carries only
-- whether one is set.
CREATE TABLE notification_webhooks (
  id                   TEXT PRIMARY KEY NOT NULL,
  name                 TEXT NOT NULL,
  url                  TEXT NOT NULL,
  -- Only `generic` is our own contract. The other four follow whichever vendor
  -- publishes them, down to their signing schemes and their habit of reporting
  -- failure inside a 200.
  format               TEXT NOT NULL CHECK (format IN ('generic', 'dingtalk', 'feishu', 'wecom', 'slack')),
  -- A JSON array of event kinds. TEXT is storage, not a public string contract:
  -- requests and responses carry the typed array, and a value that will not
  -- decode fails the boundary rather than becoming an empty subscription.
  events               TEXT NOT NULL,
  is_enabled           INTEGER NOT NULL DEFAULT 1 CHECK (is_enabled IN (0, 1)),
  -- Delivery health. A dead endpoint has to be visible somewhere a person
  -- looks; nothing here disables it, because a notification channel that
  -- switches itself off silently is the failure this whole feature exists to
  -- avoid.
  last_attempt_at      BIGINT,
  last_success_at      BIGINT,
  last_error           TEXT,
  consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
  created_at           BIGINT NOT NULL,
  updated_at           BIGINT NOT NULL
);
CREATE INDEX idx_notification_webhooks_enabled ON notification_webhooks(is_enabled);

-- What has already been reported, per alert key.
--
-- The watcher this replaces kept the same set in a process-local HashSet, so a
-- restart forgot every alert it had sent — and a restart loop would send one
-- notification per launch to a group robot with its own rate limit.
--
-- `last_notified_at` is NULL until a delivery is actually accepted by at least
-- one enabled endpoint. Writing it when the alert is *raised* is the bug the
-- old watcher guarded against by hand: an alert marked as told, that nobody was
-- told about, is never sent again until the condition clears and returns.
--
-- `fingerprint` is what was said, so a state that gets worse — a low balance
-- becoming an unusable account — is reported again rather than suppressed as a
-- repeat of the milder one.
CREATE TABLE notification_alert_state (
  alert_key        TEXT PRIMARY KEY NOT NULL,
  first_raised_at  BIGINT NOT NULL,
  last_raised_at   BIGINT NOT NULL,
  last_notified_at BIGINT,
  fingerprint      TEXT NOT NULL
);

-- The balance threshold moves out of OneBot.
--
-- It used to be `onebot.balance_alert_threshold`, read by a watcher that lived
-- inside the chat server — so the whole feature was behind the bot being on,
-- and the QQ admins were the only possible audience. The watcher is
-- `notify::balance` now and QQ is one outlet among several, so there is one
-- threshold rather than two.
--
-- Setting it is what turned the old watcher on, so a value carried across here
-- also enables the new one: migrating the number while leaving the feature off
-- would silently stop warning somebody who had asked to be warned. The old key
-- is removed rather than left beside the new one, because two keys meaning one
-- thing is the state where they start to disagree.
INSERT OR IGNORE INTO preferences (key, value, updated_at)
SELECT 'notify.balance.threshold', value, updated_at
  FROM preferences
 WHERE key = 'onebot.balance_alert_threshold' AND value <> '';

INSERT OR IGNORE INTO preferences (key, value, updated_at)
SELECT 'notify.enabled', 'true', updated_at
  FROM preferences
 WHERE key = 'onebot.balance_alert_threshold' AND value <> '';

DELETE FROM preferences WHERE key = 'onebot.balance_alert_threshold';
