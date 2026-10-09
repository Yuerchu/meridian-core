-- Messages become a tree so regeneration can add a sibling answer instead of
-- deleting the old one. A conversation still renders as a single line: the
-- active path from the root down to `head_message_id`.

-- Deliberately a plain TEXT column with no REFERENCES clause.
--
-- Foreign keys are enforced immediately (PRAGMA foreign_keys=ON, set per pooled
-- connection), and deleting a subtree is one statement over a set of ids that
-- includes both parents and children. SQLite checks row by row, so the parent
-- row can go first and trip the constraint against children that are about to
-- be deleted in the same statement. ON DELETE CASCADE avoids that but recurses
-- once per level, and the chain runs one node per message — a long conversation
-- would exhaust SQLITE_MAX_TRIGGER_DEPTH. Integrity comes from deleting whole
-- subtrees, which is the product requirement regardless.
ALTER TABLE messages ADD COLUMN parent_id TEXT;

-- Which message a compaction summary stands in front of. Replaces the
-- conversations.compact_cursor sort_order threshold, which cannot express a
-- boundary once sibling branches interleave their sort_order ranges: a summary
-- whose anchor is off the active path is simply ignored, so a branch can never
-- be handed another branch's summary. CASCADE is safe here — this edge is one
-- level deep and never chains.
ALTER TABLE messages ADD COLUMN compact_anchor_id TEXT
  REFERENCES messages(id) ON DELETE CASCADE;

-- The leaf the active path ends at. NULL falls back to the highest sort_order
-- row, which is necessarily a leaf: any child of it would have been inserted
-- later and so carry a larger sort_order. That fallback is why this column can
-- be left empty here and still read correctly.
ALTER TABLE conversations ADD COLUMN head_message_id TEXT
  REFERENCES messages(id) ON DELETE SET NULL;

CREATE INDEX idx_messages_parent ON messages(parent_id);

-- Existing conversations are linear, so each message's parent is the row before
-- it. Ordered by (sort_order, id) because sort_order is only unique in practice,
-- not by constraint. Compaction summaries sit at sort_order -1 and are not part
-- of the conversation, so they stay off the chain entirely — including them
-- would make every first message look like it had a sibling.
UPDATE messages SET parent_id = (
  SELECT m2.id FROM messages m2
  WHERE m2.conversation_id = messages.conversation_id
    AND m2.is_compact_summary = 0
    AND (m2.sort_order < messages.sort_order
         OR (m2.sort_order = messages.sort_order AND m2.id < messages.id))
  ORDER BY m2.sort_order DESC, m2.id DESC
  LIMIT 1
) WHERE is_compact_summary = 0;

-- Carry the existing cursor over: it names a sort_order, and the anchor is the
-- first message at or past it.
UPDATE messages SET compact_anchor_id = (
  SELECT m2.id FROM messages m2
  JOIN conversations c ON c.id = m2.conversation_id
  WHERE m2.conversation_id = messages.conversation_id
    AND m2.is_compact_summary = 0
    AND c.compact_cursor IS NOT NULL
    AND m2.sort_order >= c.compact_cursor
  ORDER BY m2.sort_order ASC, m2.id ASC
  LIMIT 1
) WHERE is_compact_summary = 1;

UPDATE conversations SET head_message_id = (
  SELECT m.id FROM messages m
  WHERE m.conversation_id = conversations.id
    AND m.is_compact_summary = 0
  ORDER BY m.sort_order DESC, m.id DESC
  LIMIT 1
);
