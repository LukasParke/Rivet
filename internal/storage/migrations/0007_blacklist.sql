-- Blacklist entries (avoid repeating known-bad releases)

CREATE TABLE IF NOT EXISTS blacklist_entries (
  id TEXT PRIMARY KEY,
  scope TEXT NOT NULL, -- 'global' | 'target'
  target_id TEXT,      -- required when scope='target'
  match_type TEXT NOT NULL, -- 'hash' | 'url' | 'title'
  match_value TEXT NOT NULL,
  reason TEXT,
  created_by_user_id TEXT,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_blacklist_match ON blacklist_entries(match_type, match_value);
CREATE INDEX IF NOT EXISTS idx_blacklist_target ON blacklist_entries(target_id);


