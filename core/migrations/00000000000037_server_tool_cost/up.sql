-- What the provider's own tools cost, which is not a token price.
--
-- A request that lets Grok search bills two ways: the tokens, and $5 per 1000
-- invocations on top. Measured on `grok-4.6`, one search added $0.005 to a reply
-- whose tokens came to $0.0078 — so a searching turn was being reported at
-- roughly two thirds of what it cost, with nothing to say so.
--
-- One rate rather than one per tool. xAI charges $5/1k for all three of the
-- tools this app can ask for (`web_search`, `x_search`, `code_execution`); the
-- ones priced differently — `attachment_search` at $10, `collections_search` at
-- $2.50 — are ones it never requests, and `view_image` and remote MCP calls
-- carry no invocation charge at all. A per-tool table would be a column with one
-- distinct value in it, and the day that stops being true is the day this needs
-- widening anyway.
--
-- Per 1000 calls, matching how the upstream publishes it. The token prices
-- beside it are per million for the same reason: a column whose unit differs
-- from the source it is copied from is a transcription error waiting to happen.
--
-- NULL means nobody has said, which is the state every existing row is in and
-- the only honest answer for a model whose provider-side tools are switched off.
ALTER TABLE model_configs ADD COLUMN server_tool_price REAL;

-- How many of them this reply actually made.
--
-- On `messages` because that is where the other four counts live and where the
-- audit copy reads from — `audit::record` is handed a row and nothing else, so a
-- figure that never reached the transcript cannot reach the ledger either.
-- Counted per reply rather than per turn, like the token columns beside it, so
-- the cost lands on the round that incurred it.
--
-- Already narrowed to the invocations that carry a charge; the upstream itemises
-- them and several kinds are free. NULL means none ran or none were reported,
-- which is the state of every row written before this.
ALTER TABLE messages ADD COLUMN server_tool_calls INTEGER;

-- The same two facts, snapshotted onto the audit row for the reason migration 30
-- gives: this table says what happened, and what a thing cost at the time is
-- part of that.
--
-- The count is what the upstream reported for *this* request, already narrowed
-- to the invocations that carry a charge — `usage.server_side_tool_usage_details`
-- itemises them, and the free ones (image understanding, MCP) are excluded on the
-- way in rather than here.
--
-- Both nullable and not backfilled, following migrations 28 and 30. NULL in the
-- count means "no provider-side tool ran, or nothing was reported"; NULL in the
-- price means the row predates this or the model was never priced for it. A
-- reader must not read either as zero — see `Prices::known`.
ALTER TABLE audit_messages ADD COLUMN server_tool_calls INTEGER;
ALTER TABLE audit_messages ADD COLUMN server_tool_price REAL;
