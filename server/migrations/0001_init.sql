CREATE TABLE users (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  username TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  ctu_username TEXT NOT NULL,
  ctu_password_ciphertext TEXT NOT NULL,
  telegram_chat_id INTEGER,
  telegram_username TEXT,
  telegram_link_hash TEXT,
  telegram_link_expires INTEGER,
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE sessions (
  token_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  expires_at INTEGER NOT NULL
);
CREATE INDEX sessions_user_idx ON sessions(user_id);
CREATE TABLE login_attempts (
  username TEXT NOT NULL,
  attempted_at INTEGER NOT NULL
);
CREATE INDEX login_attempts_idx ON login_attempts(username, attempted_at);
CREATE TABLE rooms (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  floor TEXT NOT NULL
);
CREATE TABLE schedule_slots (
  room_id INTEGER NOT NULL REFERENCES rooms(id),
  booking_date TEXT NOT NULL,
  slot INTEGER NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('available','reserved','unavailable','closed')),
  PRIMARY KEY(room_id, booking_date, slot)
);
CREATE TABLE sync_status (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  synced_at INTEGER NOT NULL,
  source TEXT NOT NULL
);
CREATE TABLE bookings (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL REFERENCES users(id),
  room_id INTEGER NOT NULL REFERENCES rooms(id),
  booking_date TEXT NOT NULL,
  slot INTEGER NOT NULL,
  students INTEGER NOT NULL CHECK(students BETWEEN 5 AND 20),
  note TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('pending', 'confirmed', 'failed', 'uncertain')),
  message TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  completed_at INTEGER
);
-- Guards local double booking, including uncertain upstream outcomes.
CREATE UNIQUE INDEX active_room_slot ON bookings(room_id,booking_date,slot)
  WHERE status IN ('pending', 'confirmed', 'uncertain');
CREATE UNIQUE INDEX active_user_slot ON bookings(user_id,booking_date,slot)
  WHERE status IN ('pending', 'confirmed', 'uncertain');
CREATE INDEX bookings_user_idx ON bookings(user_id,created_at DESC);
CREATE TABLE notifications (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  booking_id INTEGER NOT NULL UNIQUE REFERENCES bookings(id),
  chat_id INTEGER NOT NULL,
  body TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at INTEGER NOT NULL DEFAULT (unixepoch()),
  sent_at INTEGER
);
