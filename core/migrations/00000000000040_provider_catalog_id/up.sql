-- Which vendor in the shipped catalog this row is an instance of.
--
-- Distinct from `provider_type`, and the distinction is the point.
-- `provider_type` decides *how a request is sent* — it is what
-- `create_provider` matches on to pick an adapter. `catalog_id` decides *what
-- this looks like and what a fresh row starts from*: display name, icon, the
-- page that issues keys, the prefilled base URL. Dozens of OpenAI-compatible
-- vendors share one adapter while each keeps its own identity, so tying the two
-- together would mean a match arm per vendor — which is the coupling
-- `provider_catalog.json` exists to remove.
--
-- NULL is an ordinary state, not a gap waiting to be filled. It is what a
-- hand-made provider gets, what a row pointing at a relay gets, and what
-- anything this migration cannot identify with certainty gets. Every reader
-- must cope with it: the catalog is a *creation preset*, so a row that never
-- had one works exactly as it did before.
ALTER TABLE providers ADD COLUMN catalog_id TEXT;

-- Backfill, and it is deliberately narrow.
--
-- `provider_type` alone cannot identify a vendor — `openai` covers the official
-- API, somebody's self-hosted proxy, and every compatible reseller. Guessing
-- from it would stamp a wrong identity onto a row silently, which is worse than
-- leaving it blank: NULL means "nobody said", while a wrong id means the panel
-- shows the wrong logo and offers a key page for a service the user is not
-- talking to.
--
-- So the only rows touched are the ones whose base URL is a vendor's own,
-- verbatim. A relay address matches nothing and stays NULL, which is correct —
-- we genuinely do not know whose relay it is.
--
-- These URLs are written out here rather than read from `provider_catalog.json`
-- on purpose. A migration has to replay to the same result in five years, and
-- the catalog is editable data that will have moved on by then. If a default
-- URL changes upstream, this backfill keeps matching what rows *actually hold*,
-- which is what it is for.
--
-- `rtrim(...,'/')` and `lower(...)` because a trailing slash and a capital
-- letter are the same address; nothing else is normalised, since anything
-- further starts being a guess.
--
-- `catalog_id IS NULL` on every statement so this is idempotent and can never
-- overwrite an answer something else already wrote.

UPDATE providers SET catalog_id = 'openai'
WHERE catalog_id IS NULL AND provider_type = 'openai'
  AND lower(rtrim(base_url, '/')) = 'https://api.openai.com/v1';

UPDATE providers SET catalog_id = 'anthropic'
WHERE catalog_id IS NULL AND provider_type = 'anthropic'
  AND lower(rtrim(base_url, '/')) = 'https://api.anthropic.com';

UPDATE providers SET catalog_id = 'deepseek'
WHERE catalog_id IS NULL AND provider_type = 'deepseek'
  AND lower(rtrim(base_url, '/')) = 'https://api.deepseek.com';

UPDATE providers SET catalog_id = 'xai'
WHERE catalog_id IS NULL AND provider_type = 'xai'
  AND lower(rtrim(base_url, '/')) = 'https://api.x.ai/v1';

-- Google is the one vendor with two addresses, one per dialect. Both identify
-- the same catalog entry, so both are matched — "exactly one entry" is a rule
-- about the vendor, not about the URL.
UPDATE providers SET catalog_id = 'google'
WHERE catalog_id IS NULL AND provider_type = 'google'
  AND lower(rtrim(base_url, '/')) IN (
    'https://generativelanguage.googleapis.com',
    'https://generativelanguage.googleapis.com/v1beta/openai'
  );
