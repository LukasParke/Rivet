-- Add per-session CSRF token to support cookie-auth CSRF protection.

ALTER TABLE sessions ADD COLUMN csrf_token TEXT;

CREATE INDEX IF NOT EXISTS idx_sessions_csrf_token ON sessions(csrf_token);
