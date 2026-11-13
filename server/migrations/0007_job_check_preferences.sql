-- Persist per-job preferred first check time (opens_at) and retry cadence.
-- Existing prebook jobs keep their current next run and 5-minute checks.
ALTER TABLE scheduled_jobs ADD COLUMN retry_interval_minutes INTEGER NOT NULL DEFAULT 5
  CHECK(retry_interval_minutes IN (5,15,30,60,120,180,360));
