-- Existing scheduled jobs remain one-time manual attempts.
ALTER TABLE scheduled_jobs ADD COLUMN kind TEXT NOT NULL DEFAULT 'manual' CHECK(kind IN ('manual','prebook'));
ALTER TABLE scheduled_jobs ADD COLUMN opens_at INTEGER;
ALTER TABLE scheduled_jobs ADD COLUMN retry_until INTEGER;
ALTER TABLE scheduled_jobs ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
