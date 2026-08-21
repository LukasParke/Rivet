-- Acquisition verification + stall/recovery fields

ALTER TABLE acquisitions ADD COLUMN verified_path TEXT;
ALTER TABLE acquisitions ADD COLUMN quarantined_path TEXT;
ALTER TABLE acquisitions ADD COLUMN verify_status TEXT;

ALTER TABLE acquisitions ADD COLUMN download_started_at TEXT;
ALTER TABLE acquisitions ADD COLUMN progress_changed_at TEXT;
ALTER TABLE acquisitions ADD COLUMN stalled_at TEXT;
ALTER TABLE acquisitions ADD COLUMN recovery_attempt INTEGER;
