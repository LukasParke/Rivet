-- Covering indexes for hot storage paths.
--
-- idx_jobs_claimable serves the queue-claim candidate scan
-- (src/storage/jobs.rs `claim_one`): equality on `state`, then the
-- `ORDER BY run_after, created_at` walk straight from the index, with
-- `lease_expires_at` as a trailing key column so the lease-expiry check
-- never touches the table. The pre-existing idx_jobs_kind_state_run_after
-- leads with `kind`, which claim_one never binds, so it cannot serve this.
--
-- idx_episodes_missing serves the missing-episode scan
-- (src/storage/series.rs `list_missing_episodes`): equality on
-- `monitored`, `has_file`, range on `air_date_utc` — the filter and the
-- output ordering both come from the index, eliminating the sort. The
-- pre-existing idx_episodes_monitored(monitored, has_file) lacks the
-- ordering column; idx_episodes_air_date lacks the filter columns.
CREATE INDEX IF NOT EXISTS idx_jobs_claimable
  ON jobs(state, run_after, created_at, lease_expires_at);
CREATE INDEX IF NOT EXISTS idx_episodes_missing
  ON episodes(monitored, has_file, air_date_utc);
