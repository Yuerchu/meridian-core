-- What each of those tokens cost, at the price it cost then.
--
-- `audit_messages` has recorded four token counts since migration 29 and
-- `model_configs` has held prices since migration 15, so a bill can already be
-- produced -- by joining the two at read time. That join answers the wrong
-- question. Prices are edited: a provider cuts its rate, a user corrects a typo
-- in one, a model is re-pointed at a cheaper tier. Every one of those silently
-- rewrites what last month cost, and the number that changes is the one someone
-- was using to decide whether to keep paying for it.
--
-- So the price is copied onto the row, the same way `provider_name` and
-- `sender_name` already are, and for the same reason: this table answers "what
-- happened", and what a thing cost at the time is part of what happened.
--
-- Four columns rather than one `cost`, because a single total cannot be taken
-- apart again. The breakdown the app already shows -- input, output, cache --
-- has to be reconstructible, and a formula that changes must be able to be
-- re-run over history. Keeping the inputs and one implementation of the formula
-- (`agent::pricing::compute_cost`) is what stops the dashboard and the turn's
-- own stop event from quietly disagreeing.
--
-- Nullable and not backfilled, following migration 28. NULL means "recorded
-- before prices were kept", and a reader falls back to whatever `model_configs`
-- says today -- which is exactly the retroactive number this migration exists
-- to avoid, but it is the only number that exists for those rows and saying so
-- is better than reporting zero.
ALTER TABLE audit_messages ADD COLUMN input_price REAL;
ALTER TABLE audit_messages ADD COLUMN output_price REAL;
ALTER TABLE audit_messages ADD COLUMN cache_read_price REAL;
ALTER TABLE audit_messages ADD COLUMN cache_write_price REAL;

-- What a cache write costs, which is not what input costs.
--
-- `pricing.rs` has billed writes at `input_price` since cache accounting
-- landed, with a comment saying to add this column in the change that needs it.
-- This is that change. Anthropic charges 1.25x input for a five-minute cache
-- entry and 2x for an hour, so a run that builds a large cache is understated
-- by that premium today.
--
-- Nullable, meaning "price a write like ordinary input" -- which is both the
-- behaviour every existing row already has and the right default for the
-- providers that do not charge a premium at all. Not a multiplier: the column
-- is a price per million tokens like the three beside it, because a mixture of
-- absolute prices and factors is a unit error waiting to be made.
ALTER TABLE model_configs ADD COLUMN cache_write_price REAL;

-- Which bot account answered.
--
-- `turn_origin` separates desktop traffic from bot traffic and `source_id` says
-- which group or private chat it was, but neither says which account was
-- logged in -- the OneBot config is a single listener today, and `self_id`
-- arrives on every event and is thrown away. That is fine right up until a
-- second account connects to the same port, at which point a month of history
-- cannot be split and never will be able to be: the fact is only knowable while
-- the event is in hand.
--
-- On `turns` because that is where `origin` lives and this is the same kind of
-- fact about the same thing; copied onto `audit_messages` because `turns`
-- cascades with its conversation and the audit log does not.
--
-- NULL on every desktop turn, which is a statement rather than a gap.
ALTER TABLE turns ADD COLUMN self_id BIGINT;
ALTER TABLE audit_messages ADD COLUMN self_id BIGINT;

-- Reports are "a window of time, grouped by one thing". `created_at` already
-- has an index for the window; this one is for the grouping, and covers the
-- common case of a report narrowed to one upstream.
CREATE INDEX idx_audit_provider_model ON audit_messages(provider_id, model_id);
