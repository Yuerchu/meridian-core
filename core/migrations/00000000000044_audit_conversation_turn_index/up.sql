-- Conversation snapshots aggregate durable usage by turn. Without this index
-- opening one conversation scans the entire append-only audit ledger.
CREATE INDEX idx_audit_conversation_turn
    ON audit_messages(conversation_id, turn_id);
