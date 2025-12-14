-- Add fields to support monitoring and import

ALTER TABLE acquisitions ADD COLUMN source_url TEXT;
ALTER TABLE acquisitions ADD COLUMN content_path TEXT;
ALTER TABLE acquisitions ADD COLUMN imported_path TEXT;
ALTER TABLE acquisitions ADD COLUMN completed_at TEXT;
ALTER TABLE acquisitions ADD COLUMN imported_at TEXT;
ALTER TABLE acquisitions ADD COLUMN progress REAL;
