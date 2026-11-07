-- An older version did not record whether a CTU POST might have happened.
-- Keep old in-flight jobs conservative: they must not be replayed blindly.
ALTER TABLE scheduled_jobs ADD COLUMN execution_phase TEXT NOT NULL DEFAULT 'legacy_unknown'
  CHECK(execution_phase IN ('legacy_unknown','preflight','submission_started','completed'));
ALTER TABLE scheduled_jobs ADD COLUMN started_at INTEGER;
ALTER TABLE scheduled_jobs ADD COLUMN finished_at INTEGER;
ALTER TABLE scheduled_jobs ADD COLUMN recovered_at INTEGER;
ALTER TABLE scheduled_jobs ADD COLUMN recovery_count INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS scheduled_jobs_recovery ON scheduled_jobs(status,execution_phase);
