-- Unlike the column migrations either side of it, this one really does drop
-- what it made. A whole table an older build has never heard of is inert rather
-- than merely unread, and leaving it would strand rows that nothing can add to,
-- purge, or explain.
--
-- That does destroy records nothing else holds a copy of. Reverting past this
-- point is therefore a decision about retention, not just about schema — take
-- the export first if the deployment answers to anyone for what it did.
DROP INDEX IF EXISTS idx_audit_sender;
DROP INDEX IF EXISTS idx_audit_created;
DROP TABLE IF EXISTS audit_messages;
