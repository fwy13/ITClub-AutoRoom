-- The anonymous CTU timetable distinguishes a reserved shift from an empty
-- non-clickable shift (`catrong_khl`). Preserve the distinction in SQLite.

CREATE TABLE schedule_slots_new (
  room_id INTEGER NOT NULL REFERENCES rooms(id),
  booking_date TEXT NOT NULL,
  slot INTEGER NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('available','vacant','reserved','unavailable','closed')),
  PRIMARY KEY(room_id, booking_date, slot)
);
INSERT INTO schedule_slots_new(room_id,booking_date,slot,status)
  SELECT room_id,booking_date,slot,status FROM schedule_slots;
DROP TABLE schedule_slots;
ALTER TABLE schedule_slots_new RENAME TO schedule_slots
