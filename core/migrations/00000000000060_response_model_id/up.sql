-- The model the upstream actually used for this reply, as reported in the
-- response.  NULL when the provider did not include one (some relays strip it)
-- or for rows written before this column existed.  Compared against `model_id`
-- (the requested model) to detect silent substitutions by proxies or load
-- balancers.
ALTER TABLE messages ADD COLUMN response_model_id TEXT;
ALTER TABLE audit_messages ADD COLUMN response_model_id TEXT;
