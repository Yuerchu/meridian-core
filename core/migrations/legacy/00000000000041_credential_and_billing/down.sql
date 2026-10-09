-- Reverting loses the distinction between "unpriced because nobody said" and
-- "unpriced because there is no per-request price", which is the whole point of
-- the column — every row goes back to being counted as unaccounted-for cost.
ALTER TABLE audit_messages DROP COLUMN billing_mode;
ALTER TABLE providers DROP COLUMN transport_profile;
ALTER TABLE providers DROP COLUMN credential_kind;
