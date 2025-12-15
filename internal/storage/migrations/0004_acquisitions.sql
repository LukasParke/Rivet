-- Acquisitions (unified torrents + usenet) v0

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS acquisitions (
  id TEXT PRIMARY KEY,
  candidate_id TEXT NOT NULL REFERENCES release_candidates(id) ON DELETE CASCADE,
  target_id TEXT NOT NULL REFERENCES wanted_targets(id) ON DELETE CASCADE,
  kind TEXT NOT NULL, -- torrent|usenet
  status TEXT NOT NULL, -- pending|enqueued|failed
  client_kind TEXT NOT NULL, -- qbittorrent|nzbget|sabnzbd
  client_name TEXT NOT NULL,
  external_id TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_acquisitions_candidate_id ON acquisitions(candidate_id);
CREATE INDEX IF NOT EXISTS idx_acquisitions_status ON acquisitions(status);
CREATE INDEX IF NOT EXISTS idx_acquisitions_target_id ON acquisitions(target_id);
