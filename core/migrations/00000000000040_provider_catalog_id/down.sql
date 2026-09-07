-- The identity goes; the row keeps working. Nothing downstream of this column is
-- load-bearing — it decides what a panel draws and what a new row is prefilled
-- with, never how a request is sent.
ALTER TABLE providers DROP COLUMN catalog_id;
