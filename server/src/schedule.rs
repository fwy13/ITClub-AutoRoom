use crate::{error::ApiError, state::AppState};
use axum::http::StatusCode;
use chrono::{NaiveDate, Utc};
use scraper::{Html, Selector};
use serde::Serialize;
use sqlx::Row;

#[derive(Debug, Clone, Serialize)]
pub struct Slot {
    pub room_id: i64,
    pub date: String,
    pub slot: i64,
    pub status: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct Room {
    pub id: i64,
    pub name: String,
    pub floor: String,
}
#[derive(Debug, Clone)]
pub struct Schedule { pub rooms: Vec<Room>, pub slots: Vec<Slot> }

pub fn parse(html: &str) -> Result<Schedule, String> {
    let document = Html::parse_document(html);
    let rooms_sel = Selector::parse(".phong").unwrap();
    let name_sel = Selector::parse(".tenphong").unwrap();
    let day_sel = Selector::parse(".ngay").unwrap();
    let date_sel = Selector::parse(".tenngay").unwrap();
    let cell_sel = Selector::parse(".ca, .chunhat").unwrap();
    let mut rooms = Vec::new();
    let mut slots = Vec::new();
    for room_el in document.select(&rooms_sel) {
        let name_el = room_el.select(&name_sel).next().ok_or("Missing room name")?;
        let name = name_el.text().collect::<Vec<_>>().join(" ").trim().to_string();
        let id = name_el.value().classes().find_map(|c| c.strip_prefix("phong")?.parse::<i64>().ok()).ok_or("Missing room ID class")?;
        if !(1..=6).contains(&id) { return Err("Unknown room ID".into()); }
        if rooms.iter().any(|r: &Room| r.id == id) { return Err("Duplicate room ID".into()); }
        let (start, count) = match id { 1..=3 => (1i64,6usize), 4 => (7,3), _ => (10,4) };
        let floor = if id <= 3 { "2" } else { "3" };
        let mut days = 0;
        for day_el in room_el.select(&day_sel) {
            let date_el = day_el.select(&date_sel).next().ok_or("Missing date cell")?;
            let date_string = date_el.value().attr("title").ok_or("Missing date title")?;
            let date = NaiveDate::parse_from_str(date_string, "%d-%m-%Y").map_err(|_| "Invalid source date")?.format("%Y-%m-%d").to_string();
            let cells = day_el.select(&cell_sel).collect::<Vec<_>>();
            if cells.len() != count { return Err(format!("Unexpected slot count for room {id}: {}",cells.len())); }
            for (i, cell) in cells.into_iter().enumerate() {
                let classes = cell.value().classes().collect::<Vec<_>>();
                // CTU uses 'catrong_khl' for EMPTY cells that are not clickable
                // in this view (for example, before a user logs in). That does
                // not mean a reservation exists. Keep occupancy separate from
                // confirmed eligibility. Never promote unknown/expired cells.
                let status = if classes.contains(&"caban") { "reserved" }
                    else if classes.contains(&"chunhat") { "closed" }
                    else if classes.contains(&"catrong_quahan") { "unavailable" }
                    else if classes.contains(&"catrong_hl") { "available" }
                    else if classes.contains(&"catrong_khl") { "vacant" }
                    else { "unavailable" };
                let slot_id = start + i as i64;
                // If CTU exposes explicit POST attributes, do not accept conflicting mappings.
                if status == "available" {
                    if let Some(remote_slot) = cell.value().attr("dkca") {
                        if remote_slot.parse::<i64>().ok() != Some(slot_id) { return Err("CTU slot mapping changed".into()); }
                    }
                    if let Some(remote_room) = cell.value().attr("dkidp") {
                        if remote_room.parse::<i64>().ok() != Some(id) { return Err("CTU room mapping changed".into()); }
                    }
                }
                slots.push(Slot { room_id:id, date:date.clone(), slot:slot_id, status:status.to_string() });
            }
            days += 1;
        }
        if days < 7 { return Err(format!("Incomplete calendar for room {id}")); }
        rooms.push(Room { id, name, floor:floor.to_string() });
    }
    if rooms.len() != 6 { return Err(format!("Expected 6 CTU rooms; got {}",rooms.len())); }
    Ok(Schedule {rooms, slots})
}

pub async fn fetch_live(state: &AppState) -> Result<Schedule, ApiError> {
    let started = std::time::Instant::now();
    let url = format!("{}/lich.php", state.cfg.ctu_base.trim_end_matches('/'));
    let response = state.http.get(&url).header("Accept", "text/html").send().await
        .map_err(|e| { tracing::warn!(error=%e,"CTU schedule unavailable"); ApiError::unavailable("Cannot reach CTU schedule; booking is disabled") })?;
    if !response.status().is_success() {
        tracing::warn!(stage="public_timetable", http_status=response.status().as_u16(),
            elapsed_ms=started.elapsed().as_millis() as u64,
            "CTU public timetable HTTP request rejected");
        return Err(ApiError::unavailable("CTU schedule returned an error"));
    }
    let body = response.text().await.map_err(|_| ApiError::unavailable("Cannot decode CTU schedule"))?;
    let calendar = parse(&body).map_err(|e| {
        tracing::warn!(stage="public_timetable", parse_error=%e,"CTU schedule format changed");
        ApiError::unavailable("CTU schedule format is not recognized; booking is disabled")
    })?;
    tracing::debug!(stage="public_timetable", room_count=calendar.rooms.len(),
        slot_count=calendar.slots.len(), elapsed_ms=started.elapsed().as_millis() as u64,
        "CTU timetable fetched and validated");
    Ok(calendar)
}

pub async fn save(state: &AppState, schedule: &Schedule, source: &str) -> Result<(), ApiError> {
    let mut tx = state.db.begin().await?;
    // Replace only after an entire source page has passed validation.
    sqlx::query("DELETE FROM schedule_slots").execute(&mut *tx).await?;
    for room in &schedule.rooms {
        sqlx::query("INSERT INTO rooms (id,name,floor) VALUES (?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,floor=excluded.floor")
            .bind(room.id).bind(&room.name).bind(&room.floor).execute(&mut *tx).await?;
    }
    for slot in &schedule.slots {
        sqlx::query("INSERT INTO schedule_slots (room_id,booking_date,slot,status) VALUES (?,?,?,?)")
            .bind(slot.room_id).bind(&slot.date).bind(slot.slot).bind(&slot.status).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO sync_status (id,synced_at,source) VALUES(1,?,?) ON CONFLICT(id) DO UPDATE SET synced_at=excluded.synced_at,source=excluded.source")
        .bind(Utc::now().timestamp()).bind(source).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn refresh(state: &AppState) -> Result<Schedule, ApiError> {
    let _guard = state.sync_gate.lock().await;
    let schedule = fetch_live(state).await?;
    save(state,&schedule,"CTU live schedule").await?;
    Ok(schedule)
}

#[derive(Serialize)]
pub struct ScheduleResponse {
    pub rooms: Vec<Room>,
    pub slots: Vec<Slot>,
    pub synced_at: Option<i64>,
    pub source: Option<String>,
    pub stale: bool,
}
pub async fn load(state: &AppState) -> Result<ScheduleResponse, ApiError> {
    let rooms = sqlx::query("SELECT id,name,floor FROM rooms ORDER BY id").fetch_all(&state.db).await?
        .into_iter().map(|r| Room {id:r.get("id"),name:r.get("name"),floor:r.get("floor")}).collect();
    let slots = sqlx::query("SELECT room_id,booking_date,slot,status FROM schedule_slots ORDER BY booking_date,room_id,slot").fetch_all(&state.db).await?
        .into_iter().map(|r| Slot {room_id:r.get("room_id"),date:r.get("booking_date"),slot:r.get("slot"),status:r.get("status")}).collect();
    let sync = sqlx::query("SELECT synced_at,source FROM sync_status WHERE id=1").fetch_optional(&state.db).await?;
    let synced_at = sync.as_ref().map(|r|r.get::<i64,_>("synced_at"));
    let source = sync.as_ref().map(|r|r.get::<String,_>("source"));
    let stale = synced_at.map(|t| Utc::now().timestamp()-t > (state.cfg.refresh_secs as i64)*2 || source.as_deref() != Some("CTU live schedule")).unwrap_or(true);
    Ok(ScheduleResponse { rooms,slots,synced_at,source,stale })
}

pub async fn maybe_seed(state: &AppState) -> Result<(), ApiError> {
    if !state.cfg.seed_fixture { return Ok(()); }
    let present: i64 = sqlx::query_scalar("SELECT count(*) FROM sync_status").fetch_one(&state.db).await?;
    if present == 0 {
        let fixture = include_str!("../../fixtures/lich-2026-11-05.html");
        let snapshot = parse(fixture).map_err(|e| {tracing::error!(error=%e); ApiError(StatusCode::INTERNAL_SERVER_ERROR,"Invalid embedded fixture".into())})?;
        save(state,&snapshot,"Uploaded snapshot (offline, not live)").await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_uploaded_snapshot_and_does_not_book_unavailable_slots() {
        let s = parse(include_str!("../../fixtures/lich-2026-11-05.html")).unwrap();
        assert_eq!(s.rooms.len(),6);
        assert_eq!(s.slots.len(),406);
        assert_eq!(s.slots.iter().find(|x|x.room_id==1 && x.date=="2026-11-05" && x.slot==1).unwrap().status,"reserved");
        assert_eq!(s.slots.iter().find(|x|x.room_id==1 && x.date=="2026-11-13" && x.slot==1).unwrap().status,"vacant");
        assert_eq!(s.slots.iter().find(|x|x.room_id==1 && x.date=="2026-11-05" && x.slot==5).unwrap().status,"unavailable");
        assert_eq!(s.slots.iter().find(|x|x.room_id==1 && x.date=="2026-11-08" && x.slot==1).unwrap().status,"closed");
        assert_eq!(s.slots.iter().filter(|x| x.status=="available").count(),0);
        assert!(s.slots.iter().any(|x| x.status=="vacant"));
    }
}
