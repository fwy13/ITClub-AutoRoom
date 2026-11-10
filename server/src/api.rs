use crate::{auth, crypto, ctu::{self,Outcome}, error::ApiError, schedule, state::AppState, telegram, window};
use axum::{extract::{Path,State}, http::{HeaderMap,StatusCode}, Json};
use chrono::{Timelike,Duration,Utc,NaiveDate};
use serde::{Deserialize,Serialize};
use serde_json::{Value,json};
use sqlx::Row;

#[derive(Deserialize, Clone)]
pub struct BookRequest {
    pub room_id:i64, pub date:String, pub slot:i64, pub students:i64, pub note:String, pub borrow_tv:bool
}
#[derive(Serialize)]
pub struct Booking {
    pub(crate) id:i64, room_id:i64, room_name:String, date:String, slot:i64, students:i64,
    note:String, pub(crate) status:String, pub(crate) message:String, created_at:i64
}

pub async fn health(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    use std::sync::atomic::Ordering;
    let last = state.scheduler_heartbeat.load(Ordering::Relaxed);
    let db_ok = state.scheduler_last_db_ok.load(Ordering::Relaxed);
    if last <= 0 || Utc::now().timestamp() - last > 90 || Utc::now().timestamp() - db_ok > 150 {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.db).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(json!({"ok":true,"service":"ITClub-AutoRoom","scheduler_heartbeat":last,"last_scheduler_db_scan":db_ok})))
}
pub async fn get_schedule(State(state):State<AppState>)->Result<Json<schedule::ScheduleResponse>,ApiError>{
    Ok(Json(schedule::load(&state).await?))
}
pub async fn refresh_schedule(State(state):State<AppState>,headers:HeaderMap)->Result<Json<schedule::ScheduleResponse>,ApiError>{
    auth::check_origin(&headers,&state)?;
    let _=auth::authenticate(&state,&headers).await?;
    // Interactive refresh; booking itself always fetches a new source independently.
    schedule::refresh(&state).await?;
    Ok(Json(schedule::load(&state).await?))
}
pub async fn my_bookings(State(state):State<AppState>,headers:HeaderMap)->Result<Json<Vec<Booking>>,ApiError>{
    let u=auth::authenticate(&state,&headers).await?;
    let rows=sqlx::query("SELECT b.id,b.room_id,r.name AS room_name,b.booking_date,b.slot,b.students,b.note,CASE b.cancellation_status WHEN 'processing' THEN 'cancelling' WHEN 'uncertain' THEN 'cancel_uncertain' WHEN 'cancelled' THEN 'cancelled' ELSE b.status END AS status,b.message,b.created_at FROM bookings b JOIN rooms r ON r.id=b.room_id WHERE b.user_id=? ORDER BY b.created_at DESC,b.id DESC LIMIT 100")
        .bind(u.id).fetch_all(&state.db).await?;
    let bookings=rows.into_iter().map(|r|Booking{
        id:r.get("id"),room_id:r.get("room_id"),room_name:r.get("room_name"),date:r.get("booking_date"),slot:r.get("slot"),
        students:r.get("students"),note:r.get("note"),status:r.get("status"),message:r.get("message"),created_at:r.get("created_at")
    }).collect();
    Ok(Json(bookings))
}

pub async fn create_booking(State(state):State<AppState>,headers:HeaderMap,Json(p):Json<BookRequest>)->Result<(StatusCode,Json<Booking>),ApiError>{
    auth::check_origin(&headers,&state)?;
    let user=auth::authenticate(&state,&headers).await?;
    let booking=submit_for_user(&state,user,p).await?;
    Ok((StatusCode::CREATED,Json(booking)))
}

pub async fn submit_for_user(state:&AppState,user:auth::User,p:BookRequest)->Result<Booking,ApiError>{
    submit_for_user_with_job(state, user, p, None).await
}

/// For scheduled requests, persist the tentative booking and link it to its job
/// in ONE SQLite transaction, before a request can reach CTU. The link allows
/// deterministic recovery after an app crash, without replaying a possible POST.
pub async fn submit_for_user_with_job(state:&AppState,user:auth::User,p:BookRequest,job_id:Option<i64>)->Result<Booking,ApiError>{
    tracing::info!(stage="validation", job_id=?job_id, user_id=user.id,
        room_id=p.room_id, booking_date=%p.date, shift=p.slot,
        "Starting booking validation");
    if !(5..=20).contains(&p.students){return Err(ApiError::bad("Group size must be 5–20"));}
    if p.note.chars().count()>200 {return Err(ApiError::bad("Note is too long"));}
    let date=NaiveDate::parse_from_str(&p.date,"%Y-%m-%d").map_err(|_|ApiError::bad("Use YYYY-MM-DD date"))?;
    let vn_today=window::today_vn(Utc::now());
    match window::classify(date, vn_today) {
        window::Window::Immediate => {},
        window::Window::Advance => return Err(ApiError::bad("For dates A+14 through A+44, create an advance booking")),
        window::Window::Outside => return Err(ApiError::bad("Immediate bookings are allowed from today through A+13 only")),
    }
    let shift_valid=match p.room_id {1..=3 => (1..=6).contains(&p.slot),4 => (7..=9).contains(&p.slot),5..=6 => (10..=13).contains(&p.slot),_=>false};
    if !shift_valid {return Err(ApiError::bad("Invalid room/shift combination"));}
    // For the six standard slots, also reject shifts that have already finished today.
    if date==vn_today && p.slot<=6 {
        let end_minutes=[570,700,830,960,1090,1220][(p.slot-1) as usize];
        let now=Utc::now()+Duration::hours(7);
        if now.hour() as i64 * 60+now.minute() as i64>=end_minutes {
            return Err(ApiError::bad("This shift has already ended"));
        }
    }
    let note=if p.borrow_tv {format!("Mượn Tivi. {}",p.note.trim())} else {p.note.trim().to_owned()};
    // Validate the encryption key before holding a local slot.
    let ctu_password=crypto::decrypt(&state.cfg.encryption_key,&user.ctu_password_ciphertext)
        .map_err(|e|{tracing::error!(error=%e,"cannot decrypt CTU credential");ApiError::internal()})?;
    // Force a fresh upstream read; cached snapshots never authorize a booking.
    let live=schedule::refresh(&state).await?;
    let source_slot=live.slots.iter().find(|s|s.room_id==p.room_id && s.date==p.date && s.slot==p.slot).cloned()
        .ok_or_else(||ApiError::bad("Slot is not present in the current CTU calendar"))?;
    // The public CTU timetable can mark an empty cell `catrong_khl` when
    // it is not clickable for an anonymous visitor. This is NOT a reservation.
    // The actual booking flow rechecks the timetable after CTU login and
    // only CTU's own confirmation is treated as a successful reservation.
    tracing::info!(stage="public_timetable", job_id=?job_id, room_id=p.room_id,
        booking_date=%p.date, shift=p.slot, slot_status=%source_slot.status,
        "CTU public timetable occupancy check");
    if !matches!(source_slot.status.as_str(), "available" | "vacant") {
        tracing::warn!(stage="public_timetable", job_id=?job_id, room_id=p.room_id,
            booking_date=%p.date, shift=p.slot, slot_status=%source_slot.status,
            "Booking blocked by CTU occupancy state");
        return Err(ApiError::conflict("CTU reports this shift as reserved, closed, expired, or unknown"));
    }
    let room_name=live.rooms.iter().find(|r|r.id==p.room_id).map(|r|r.name.clone()).ok_or_else(ApiError::internal)?;
    // SQLite partial unique indexes provide cross-request room/slot protection.
    // The mutex also serializes same-user checks with mixed room categories.
    let _gate=state.booking_gate.lock().await;
    let conflict:i64=sqlx::query_scalar("SELECT count(*) FROM bookings WHERE user_id=? AND booking_date=? AND status IN ('pending','confirmed','uncertain') AND cancellation_status <> 'cancelled' AND (slot=? OR slot>=7 OR ?>=7)")
        .bind(user.id).bind(&p.date).bind(p.slot).bind(p.slot).fetch_one(&state.db).await?;
    if conflict>0 {
        tracing::warn!(stage="local_conflict", job_id=?job_id, room_id=p.room_id,
            booking_date=%p.date, shift=p.slot, conflicting_bookings=conflict,
            "Booking blocked by existing local booking");
        return Err(ApiError::conflict("You already have an active or unverified reservation for this time/date"));
    }
    let queued:i64=sqlx::query_scalar("SELECT COUNT(*) FROM scheduled_jobs WHERE room_id=? AND booking_date=? AND slot=? AND status IN ('scheduled','processing') AND id != ?")
        .bind(p.room_id).bind(&p.date).bind(p.slot).bind(job_id.unwrap_or(-1)).fetch_one(&state.db).await?;
    if queued > 0 { return Err(ApiError::conflict("An advance booking already holds this room/shift locally")); }
    let mut tx = state.db.begin().await?;
    let inserted=sqlx::query("INSERT INTO bookings(user_id,room_id,booking_date,slot,students,note,status,message) VALUES(?,?,?,?,?,?,'pending','CTU submission in progress')")
        .bind(user.id).bind(p.room_id).bind(&p.date).bind(p.slot).bind(p.students).bind(&note).execute(&mut *tx).await;
    let id=match inserted {
        Ok(r)=>r.last_insert_rowid(),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation()=>return Err(ApiError::conflict("Another reservation already holds this room/time")),
        Err(e)=>return Err(e.into()),
    };
    if let Some(jid) = job_id {
        let changed = sqlx::query("UPDATE scheduled_jobs SET booking_id=?,execution_phase='submission_started' WHERE id=? AND user_id=? AND status='processing' AND execution_phase='preflight'")
            .bind(id).bind(jid).bind(user.id).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(ApiError::conflict("Scheduled attempt is no longer owned by this worker"));
        }
    }
    tx.commit().await?;
    drop(_gate);
    tracing::info!(stage="submission_started", job_id=?job_id, booking_id=id,
        room_id=p.room_id, booking_date=%p.date, shift=p.slot,
        "Local booking and submission marker persisted; CTU flow starting");
    // IMPORTANT: no automatic retries after POST. An absent response is ambiguous.
    let outcome=ctu::book(&state,&user.ctu_username,&ctu_password,&source_slot,p.students,&note,id,job_id).await;
    let (status,message)=match outcome {
        Outcome::Confirmed=>("confirmed","CTU confirmed the reservation".to_string()),
        Outcome::Failed(m)=>("failed",m),
        Outcome::Uncertain(m)=>("uncertain",m),
    };
    tracing::info!(stage="result", job_id=?job_id, booking_id=id, outcome=status,
        "CTU booking flow completed; persisting result");
    sqlx::query("UPDATE bookings SET status=?,message=?,completed_at=? WHERE id=?")
        .bind(status).bind(&message).bind(Utc::now().timestamp()).bind(id).execute(&state.db).await?;
    let status_vi = match status {
        "confirmed" => "Thành công", "failed" => "Thất bại", _ => "Chưa xác định",
    };
    let notification=format!("{} Kết quả đăng ký phòng CTU\nPhòng: {}\nNgày: {}\nCa: {}\nSố sinh viên: {}\nTrạng thái: {} ({})\nChi tiết: {}",
        if status=="confirmed"{"✅"}else if status=="failed"{"❌"}else{"⚠️"},
        room_name,p.date,p.slot,p.students,status_vi,status,message);
    // Notification delivery must never turn a confirmed CTU booking into an API error.
    if let Err(e)=telegram::enqueue(&state,id,user.id,notification).await {
        tracing::warn!(error=%e.1,booking_id=id,"Could not queue Telegram notification");
    }
    let state_for_notification=state.clone();
    tokio::spawn(async move {telegram::deliver_pending(&state_for_notification).await;});
    Ok(Booking{id,room_id:p.room_id,room_name,date:p.date,slot:p.slot,students:p.students,note,status:status.to_string(),message,created_at:Utc::now().timestamp()})
}

/// Cancel only a booking belonging to the authenticated app user, using that
/// user's own CTU credentials. A cancellation is a one-shot operation: persist
/// its intent before any network I/O and never blindly replay on restart.
pub async fn cancel_booking(
    State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    auth::check_origin(&headers, &state)?;
    let user = auth::authenticate(&state, &headers).await?;
    let _gate = state.booking_gate.lock().await;
    let row = sqlx::query(
        "SELECT room_id,booking_date,slot,status,cancellation_status FROM bookings WHERE id=? AND user_id=?"
    ).bind(id).bind(user.id).fetch_optional(&state.db).await?
        .ok_or_else(|| ApiError::bad("Booking not found"))?;
    let status: String = row.get("status");
    let cancellation: String = row.get("cancellation_status");
    if status != "confirmed" || cancellation != "none" {
        return Err(ApiError::conflict("Only confirmed, not-yet-cancelled reservations can be cancelled"));
    }
    // Validate credentials before marking the one-shot cancellation attempt.
    let password = crypto::decrypt(&state.cfg.encryption_key, &user.ctu_password_ciphertext)
        .map_err(|_| ApiError::internal())?;
    let now = Utc::now().timestamp();
    let changed = sqlx::query(
        "UPDATE bookings SET cancellation_status='processing',cancel_started_at=?,message='Cancellation submitted; awaiting CTU confirmation' WHERE id=? AND user_id=? AND status='confirmed' AND cancellation_status='none'"
    ).bind(now).bind(id).bind(user.id).execute(&state.db).await?;
    if changed.rows_affected() != 1 { return Err(ApiError::conflict("Cancellation has already started")); }
    drop(_gate);

    let room_id: i64 = row.get("room_id");
    let slot: i64 = row.get("slot");
    let date: String = row.get("booking_date");
    let outcome = ctu::cancel(&state, &user.ctu_username, &password,
        room_id, slot, &date).await;
    let (cancel_status, public_status, message) = match outcome {
        ctu::CancellationOutcome::Confirmed => ("cancelled", "cancelled", "CTU confirmed cancellation".to_string()),
        ctu::CancellationOutcome::Rejected(msg) => ("none", "confirmed", msg),
        ctu::CancellationOutcome::Uncertain(msg) => ("uncertain", "cancel_uncertain", msg),
    };
    // If this UPDATE fails, startup recovery will conservatively record an
    // uncertain cancellation, and the UI must not offer a second POST.
    sqlx::query(
        "UPDATE bookings SET cancellation_status=?,message=?,cancelled_at=CASE WHEN ?='cancelled' THEN ? ELSE cancelled_at END WHERE id=? AND user_id=? AND cancellation_status='processing'"
    ).bind(cancel_status).bind(&message).bind(cancel_status).bind(Utc::now().timestamp())
     .bind(id).bind(user.id).execute(&state.db).await?;
    tracing::info!(booking_id=id, user_id=user.id, outcome=public_status, "CTU cancellation result persisted");
    // Cancellation is a second event for this booking. The existing notification
    // table allows only one row per booking_id, so send this event separately.
    let notify_state = state.clone();
    let notify_user = user.id;
    let notify_message = format!("CTU cancellation #{}: {}. {}", id, public_status, message);
    tokio::spawn(async move {
        telegram::notify_job_failure(&notify_state, notify_user, notify_message).await;
    });
    Ok(Json(json!({"id":id,"status":public_status,"message":message})))
}
