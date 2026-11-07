use crate::{auth, crypto, ctu::{self,Outcome}, error::ApiError, schedule, state::AppState, telegram};
use axum::{extract::State, http::{HeaderMap,StatusCode}, Json};
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
    let rows=sqlx::query("SELECT b.id,b.room_id,r.name AS room_name,b.booking_date,b.slot,b.students,b.note,b.status,b.message,b.created_at FROM bookings b JOIN rooms r ON r.id=b.room_id WHERE b.user_id=? ORDER BY b.created_at DESC,b.id DESC LIMIT 100")
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
    if !(5..=20).contains(&p.students){return Err(ApiError::bad("Group size must be 5–20"));}
    if p.note.chars().count()>200 {return Err(ApiError::bad("Note is too long"));}
    let date=NaiveDate::parse_from_str(&p.date,"%Y-%m-%d").map_err(|_|ApiError::bad("Use YYYY-MM-DD date"))?;
    let vn_today=(Utc::now()+Duration::hours(7)).date_naive();
    if date<vn_today {return Err(ApiError::bad("Cannot book a date in the past"));}
    if date>vn_today+Duration::days(13) {return Err(ApiError::bad("CTU bookings open at most 13 days before the target date; create a pre-booking instead"));}
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
    if source_slot.status!="available" {return Err(ApiError::conflict("This slot is reserved or is not open for CTU booking"));}
    let room_name=live.rooms.iter().find(|r|r.id==p.room_id).map(|r|r.name.clone()).ok_or_else(ApiError::internal)?;
    // SQLite partial unique indexes provide cross-request room/slot protection.
    // The mutex also serializes same-user checks with mixed room categories.
    let _gate=state.booking_gate.lock().await;
    let conflict:i64=sqlx::query_scalar("SELECT count(*) FROM bookings WHERE user_id=? AND booking_date=? AND status IN ('pending','confirmed','uncertain') AND (slot=? OR slot>=7 OR ?>=7)")
        .bind(user.id).bind(&p.date).bind(p.slot).bind(p.slot).fetch_one(&state.db).await?;
    if conflict>0 {return Err(ApiError::conflict("You already have an active or unverified reservation for this time/date"));}
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
    // IMPORTANT: no automatic retries after POST. An absent response is ambiguous.
    let outcome=ctu::book(&state,&user.ctu_username,&ctu_password,&source_slot,p.students,&note).await;
    let (status,message)=match outcome {
        Outcome::Confirmed=>("confirmed","CTU confirmed the reservation".to_string()),
        Outcome::Failed(m)=>("failed",m),
        Outcome::Uncertain(m)=>("uncertain",m),
    };
    sqlx::query("UPDATE bookings SET status=?,message=?,completed_at=? WHERE id=?")
        .bind(status).bind(&message).bind(Utc::now().timestamp()).bind(id).execute(&state.db).await?;
    let notification=format!("{} CTU Library reservation\nRoom: {}\nDate: {}\nShift: {}\nStudents: {}\nStatus: {}\n{}",
        if status=="confirmed"{"✅"}else if status=="failed"{"❌"}else{"⚠️"},room_name,p.date,p.slot,p.students,status,message);
    // Notification delivery must never turn a confirmed CTU booking into an API error.
    if let Err(e)=telegram::enqueue(&state,id,user.id,notification).await {
        tracing::warn!(error=%e.1,booking_id=id,"Could not queue Telegram notification");
    }
    let state_for_notification=state.clone();
    tokio::spawn(async move {telegram::deliver_pending(&state_for_notification).await;});
    Ok(Booking{id,room_id:p.room_id,room_name,date:p.date,slot:p.slot,students:p.students,note,status:status.to_string(),message,created_at:Utc::now().timestamp()})
}
