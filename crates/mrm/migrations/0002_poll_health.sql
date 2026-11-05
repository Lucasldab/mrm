-- Per-series poll health (M9). Additive and nullable: binaries built before
-- this migration never read or write these columns and keep working.
ALTER TABLE manhwa ADD COLUMN last_ok_at TIMESTAMPTZ;
ALTER TABLE manhwa ADD COLUMN last_error TEXT;
ALTER TABLE manhwa ADD COLUMN failing_since TIMESTAMPTZ;
