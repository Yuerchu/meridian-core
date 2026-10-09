-- The turn history goes. It describes runs that only this build knows how to
-- record, and an older build would neither write nor read it.
DROP INDEX IF EXISTS idx_turns_conversation;
DROP TABLE IF EXISTS turns;

-- The two message columns stay, following migration 19's precedent for
-- `sender_id`: dropping a column means rebuilding the messages table, which is
-- a great deal riskier than leaving two nullable columns an older build will
-- never look at. They also hold real information — which turn wrote a row, and
-- whether a tool call was refused — that reverting has no reason to destroy.
