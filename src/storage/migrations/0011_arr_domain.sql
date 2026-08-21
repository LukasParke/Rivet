-- *arr-parity domain: series/seasons/episodes, movies, history,
-- custom formats, naming templates, notifications, remote path mappings.

CREATE TABLE IF NOT EXISTS series (
  id TEXT PRIMARY KEY,
  tvdb_id INTEGER,
  tmdb_id INTEGER,
  imdb_id TEXT,
  title TEXT NOT NULL,
  sort_title TEXT,
  status TEXT NOT NULL DEFAULT 'continuing', -- continuing | ended | upcoming
  overview TEXT,
  network TEXT,
  first_aired TEXT,
  genres_json TEXT,
  path TEXT NOT NULL UNIQUE,
  quality_profile TEXT NOT NULL DEFAULT 'default',
  monitored INTEGER NOT NULL DEFAULT 1,
  season_folder INTEGER NOT NULL DEFAULT 1,
  tags_json TEXT,
  added_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_series_sort ON series(sort_title);

CREATE TABLE IF NOT EXISTS seasons (
  id TEXT PRIMARY KEY,
  series_id TEXT NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  number INTEGER NOT NULL,
  monitored INTEGER NOT NULL DEFAULT 1,
  UNIQUE(series_id, number)
);

CREATE TABLE IF NOT EXISTS episodes (
  id TEXT PRIMARY KEY,
  series_id TEXT NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  season_number INTEGER NOT NULL,
  episode_number INTEGER NOT NULL,
  absolute_episode_number INTEGER,
  title TEXT,
  air_date_utc TEXT,
  runtime INTEGER,
  overview TEXT,
  monitored INTEGER NOT NULL DEFAULT 1,
  has_file INTEGER NOT NULL DEFAULT 0,
  episode_file_id TEXT,
  UNIQUE(series_id, season_number, episode_number)
);
CREATE INDEX IF NOT EXISTS idx_episodes_air_date ON episodes(air_date_utc);
CREATE INDEX IF NOT EXISTS idx_episodes_monitored ON episodes(monitored, has_file);

CREATE TABLE IF NOT EXISTS episode_files (
  id TEXT PRIMARY KEY,
  series_id TEXT NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  season_number INTEGER NOT NULL,
  episode_numbers_json TEXT NOT NULL,
  relative_path TEXT NOT NULL,
  size_bytes INTEGER NOT NULL DEFAULT 0,
  quality TEXT NOT NULL,
  quality_score INTEGER NOT NULL DEFAULT 0,
  languages_json TEXT,
  date_added TEXT NOT NULL,
  original_path TEXT
);
CREATE INDEX IF NOT EXISTS idx_episode_files_series ON episode_files(series_id);

CREATE TABLE IF NOT EXISTS movies (
  id TEXT PRIMARY KEY,
  tmdb_id INTEGER,
  imdb_id TEXT,
  title TEXT NOT NULL,
  sort_title TEXT,
  year INTEGER,
  status TEXT NOT NULL DEFAULT 'released', -- announced | in cinemas | released
  overview TEXT,
  studio TEXT,
  collection TEXT,
  genres_json TEXT,
  path TEXT NOT NULL UNIQUE,
  quality_profile TEXT NOT NULL DEFAULT 'default',
  minimum_availability TEXT NOT NULL DEFAULT 'released',
  monitored INTEGER NOT NULL DEFAULT 1,
  tags_json TEXT,
  added_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_movies_sort ON movies(sort_title);

CREATE TABLE IF NOT EXISTS history_events (
  id TEXT PRIMARY KEY,
  event_type TEXT NOT NULL, -- grabbed|imported|upgraded|failed|renamed|deleted
  series_id TEXT,
  episode_id TEXT,
  movie_id TEXT,
  acquisition_id TEXT,
  source_title TEXT NOT NULL,
  quality TEXT,
  quality_score INTEGER,
  data_json TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_history_created ON history_events(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_history_type ON history_events(event_type, created_at DESC);

CREATE TABLE IF NOT EXISTS custom_formats (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  spec_json TEXT NOT NULL,
  score INTEGER NOT NULL DEFAULT 0,
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS naming_config (
  kind TEXT PRIMARY KEY, -- series | movie
  standard_format TEXT NOT NULL,
  season_format TEXT,
  multi_episode_format TEXT,
  safe_for_windows INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS notifications (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  kind TEXT NOT NULL, -- webhook | ntfy | discord
  config_json TEXT NOT NULL,
  events_json TEXT NOT NULL, -- event-type list this sink subscribes to
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS remote_path_mappings (
  id TEXT PRIMARY KEY,
  client_path TEXT NOT NULL UNIQUE,
  local_path TEXT NOT NULL
);
