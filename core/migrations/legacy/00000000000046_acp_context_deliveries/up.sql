-- Hosted ACP sessions keep their own history, so a shell result stored in
-- Meridian's transcript has to ride the next session/prompt once. The receipt
-- is separate from context-item metadata: metadata describes the immutable
-- command result, while delivery is mutable transport state.
CREATE TABLE acp_context_deliveries (
    context_item_id TEXT PRIMARY KEY NOT NULL
        REFERENCES message_context_items(id) ON DELETE CASCADE,
    delivered_at BIGINT NOT NULL
);
