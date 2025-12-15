-- Core domain tables (MVP scaffolding)

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS libraries (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL, -- movies|tv
  name TEXT NOT NULL,
  root_paths_json TEXT NOT NULL,
  import_mode TEXT NOT NULL, -- hardlink|copy|move
  quarantine_path TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS profiles (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL, -- movies|tv
  name TEXT NOT NULL,
  rules_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS wanted_targets (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL, -- movie|episode|season_pack
  external_source TEXT NOT NULL, -- tmdb|tvdb|...
  external_id TEXT NOT NULL,
  state TEXT NOT NULL, -- wanted|searching|acquiring|imported|failed
  payload_json TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_wanted_targets_kind_state ON wanted_targets(kind, state);
CREATE INDEX IF NOT EXISTS idx_wanted_targets_external ON wanted_targets(external_source, external_id);

CREATE TABLE IF NOT EXISTS search_runs (
  id TEXT PRIMARY KEY,
  target_id TEXT NOT NULL REFERENCES wanted_targets(id) ON DELETE CASCADE,
  status TEXT NOT NULL, -- queued|running|completed|failed
  started_at TEXT,
  completed_at TEXT,
  error TEXT
);

CREATE INDEX IF NOT EXISTS idx_search_runs_target_id ON search_runs(target_id);

CREATE TABLE IF NOT EXISTS release_candidates (
  id TEXT PRIMARY KEY,
  search_run_id TEXT NOT NULL REFERENCES search_runs(id) ON DELETE CASCADE,
  indexer_name TEXT NOT NULL,
  raw_json TEXT NOT NULL,
  normalized_json TEXT,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_release_candidates_search_run_id ON release_candidates(search_run_id);

CREATE TABLE IF NOT EXISTS decisions (
  id TEXT PRIMARY KEY,
  candidate_id TEXT NOT NULL REFERENCES release_candidates(id) ON DELETE CASCADE,
  accepted INTEGER NOT NULL, -- 0/1
  score INTEGER NOT NULL,
  reasons_json TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_decisions_candidate_id ON decisions(candidate_id);
