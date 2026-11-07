CREATE TABLE scheduled_jobs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  room_id INTEGER NOT NULL REFERENCES rooms(id),
  booking_date TEXT NOT NULL,
  slot INTEGER NOT NULL,
  students INTEGER NOT NULL CHECK(students BETWEEN 5 AND 20),
  note TEXT NOT NULL,
  borrow_tv INTEGER NOT NULL DEFAULT 0,
  run_at INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'scheduled' CHECK(status IN ('scheduled','processing','confirmed','failed','uncertain','cancelled')),
  booking_id INTEGER REFERENCES bookings(id),
  message TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX scheduled_jobs_due ON scheduled_jobs(status,run_at);
CREATE INDEX scheduled_jobs_user ON scheduled_jobs(user_id,created_at DESC);
CREATE UNIQUE INDEX scheduled_jobs_unique ON scheduled_jobs(user_id,room_id,booking_date,slot)
  WHERE status IN ('scheduled','processing');
