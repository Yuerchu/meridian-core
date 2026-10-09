DROP INDEX idx_audit_provider_model;

ALTER TABLE audit_messages DROP COLUMN self_id;
ALTER TABLE turns DROP COLUMN self_id;

ALTER TABLE model_configs DROP COLUMN cache_write_price;

ALTER TABLE audit_messages DROP COLUMN cache_write_price;
ALTER TABLE audit_messages DROP COLUMN cache_read_price;
ALTER TABLE audit_messages DROP COLUMN output_price;
ALTER TABLE audit_messages DROP COLUMN input_price;
