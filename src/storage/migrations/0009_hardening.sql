-- Hardening migration: dedupe blacklist, add missing indexes, retention helpers.

-- Blacklist dedupe: repeated blacklist actions must not accumulate rows.
DELETE FROM blacklist_entries
WHERE id NOT IN (
  SELECT MIN(id) FROM blacklist_entries
  GROUP BY scope, COALESCE(target_id,''), match_type, match_value
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_blacklist_entries
  ON blacklist_entries(scope, COALESCE(target_id,''), match_type, match_value);

-- ListWantedTargets sorts by created_at; index it.
CREATE INDEX IF NOT EXISTS idx_wanted_targets_created_at ON wanted_targets(created_at DESC);

-- Reconciler scans for non-terminal acquisitions.
CREATE INDEX IF NOT EXISTS idx_acquisitions_status_updated ON acquisitions(status, updated_at);

-- Retention pruning by run completion time.
CREATE INDEX IF NOT EXISTS idx_search_runs_status ON search_runs(status, completed_at);
