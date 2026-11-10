-- SQLite cannot ALTER an existing CHECK constraint safely while other tables
-- reference bookings. Track cancellation independently of the CTU booking result.
-- A confirmed booking is only released after CTU explicitly confirms cancellation.
ALTER TABLE bookings ADD COLUMN cancellation_status TEXT NOT NULL DEFAULT 'none'
  CHECK(cancellation_status IN ('none','processing','uncertain','cancelled'));
ALTER TABLE bookings ADD COLUMN cancel_started_at INTEGER;
ALTER TABLE bookings ADD COLUMN cancelled_at INTEGER;
DROP INDEX active_room_slot;
DROP INDEX active_user_slot;
CREATE UNIQUE INDEX active_room_slot ON bookings(room_id,booking_date,slot)
  WHERE status IN ('pending','confirmed','uncertain') AND cancellation_status <> 'cancelled';
CREATE UNIQUE INDEX active_user_slot ON bookings(user_id,booking_date,slot)
  WHERE status IN ('pending','confirmed','uncertain') AND cancellation_status <> 'cancelled';
CREATE INDEX IF NOT EXISTS bookings_cancellation_recovery ON bookings(cancellation_status);
