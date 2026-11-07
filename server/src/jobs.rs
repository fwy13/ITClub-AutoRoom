use crate::{api::{self, BookRequest}, auth, error::ApiError, schedule, state::AppState, telegram};
use axum::{extract::{Path, State}, http::{HeaderMap, StatusCode}, Json};
use std::sync::atomic::Ordering;
use tokio::task::JoinSet;
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use tokio::time::{Duration as TokioDuration, Instant, MissedTickBehavior};

// Pre-booking is based on the *target booking date* in Vietnam, not the server timezone.
// CTU's precise opening hour is not contained in the uploaded HTML: users specify it.
const PREBOOK_ADVANCE_DAYS: i64 = 13;
const MAX_TARGET_DAYS: i64 = 90;
const RETRY_WINDOW_SECONDS: i64 = 180;
const RETRY_INTERVAL_SECONDS: i64 = 15;
// The normal scheduler sleeps until the next due job. This is only a recovery/health backstop.
const WATCHDOG_SCAN_SECONDS: u64 = 30;
const ERROR_BACKOFF_SECONDS: u64 = 5;
const MAX_CONCURRENT_JOBS: usize = 2;

#[derive(Deserialize)]
pub struct NewJob {
    #[serde(flatten)]
    request: BookRequest,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    run_at: Option<String>,
    #[serde(default)]
    open_time_vn: Option<String>,
}

#[derive(Serialize)]
pub struct Job {
    id: i64,
    room_id: i64,
    room_name: String,
    date: String,
    slot: i64,
    students: i64,
    note: String,
    borrow_tv: bool,
    run_at: i64,
    kind: String,
    opens_at: Option<i64>,
    retry_until: Option<i64>,
    attempts: i64,
    recovery_count: i64,
    recovered_at: Option<i64>,
    execution_phase: String,
    status: String,
    message: String,
    booking_id: Option<i64>,
}

fn vietnam_now_date(now: &DateTime<Utc>) -> NaiveDate {
    (now.to_owned() + Duration::hours(7)).date_naive()
}

fn prebook_open_timestamp(target: NaiveDate, time: &str) -> Result<i64, ApiError> {
    let open_time = NaiveTime::parse_from_str(time, "%H:%M")
        .map_err(|_| ApiError::bad("open_time_vn must be a 24-hour HH:MM time in Vietnam"))?;
    let day = target.checked_sub_signed(Duration::days(PREBOOK_ADVANCE_DAYS))
        .ok_or_else(|| ApiError::bad("Invalid target date"))?;
    let vietnam = FixedOffset::east_opt(7 * 3600).expect("valid UTC+07 offset");
    let local = day.and_time(open_time);
    Ok(local.and_local_timezone(vietnam).single().expect("valid fixed-offset datetime").timestamp())
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(p): Json<NewJob>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth::check_origin(&headers, &state)?;
    let user = auth::authenticate(&state, &headers).await?;
    let r = &p.request;
    if !(5..=20).contains(&r.students) || r.note.chars().count() > 200 {
        return Err(ApiError::bad("Invalid group size or note"));
    }
    let valid = match r.room_id {
        1..=3 => (1..=6).contains(&r.slot),
        4 => (7..=9).contains(&r.slot),
        5..=6 => (10..=13).contains(&r.slot),
        _ => false,
    };
    if !valid { return Err(ApiError::bad("Invalid room/shift combination")); }

    let date = NaiveDate::parse_from_str(&r.date, "%Y-%m-%d")
        .map_err(|_| ApiError::bad("Invalid target date"))?;
    let now = Utc::now();
    let today = vietnam_now_date(&now);
    if date < today || date > today + Duration::days(MAX_TARGET_DAYS) {
        return Err(ApiError::bad("Target date must be within the next 90 days"));
    }

    let kind = p.kind.as_deref().unwrap_or("manual");
    let (run_at, opens_at, retry_until) = match kind {
        "prebook" => {
            if p.run_at.is_some() {
                return Err(ApiError::bad("Pre-booking calculates run_at automatically; omit run_at"));
            }
            let opening = p.open_time_vn.as_deref()
                .ok_or_else(|| ApiError::bad("Choose the CTU opening time in Vietnam (HH:MM)"))?;
            let at = prebook_open_timestamp(date, opening)?;
            if at <= now.timestamp() + 10 {
                return Err(ApiError::bad("The 13-day pre-booking time has already passed; choose a later date or time"));
            }
            (at, Some(at), Some(at + RETRY_WINDOW_SECONDS))
        }
        "manual" => {
            if p.open_time_vn.is_some() {
                return Err(ApiError::bad("open_time_vn is only valid for pre-booking"));
            }
            let when = p.run_at.as_deref()
                .ok_or_else(|| ApiError::bad("run_at is required for manual scheduled attempts"))?;
            let at = DateTime::parse_from_rfc3339(when)
                .map_err(|_| ApiError::bad("run_at must be ISO8601 with a timezone"))?.timestamp();
            if at <= now.timestamp() + 10 || at > now.timestamp() + 45 * 86400 {
                return Err(ApiError::bad("Attempt time must be between 10 seconds and 45 days from now"));
            }
            // The window opens no earlier than midnight 13 days before the target date.
            // This is a lower bound; actual CTU opening hour may be later.
            let earliest = prebook_open_timestamp(date, "00:00")?;
            if at < earliest {
                return Err(ApiError::bad("Cannot schedule an attempt more than 13 days before the target date"));
            }
            (at, None, None)
        }
        _ => return Err(ApiError::bad("kind must be manual or prebook")),
    };

    let target_end = date.and_hms_opt(23, 59, 59).expect("valid date").and_utc().timestamp() - 7 * 3600;
    if run_at > target_end {
        return Err(ApiError::bad("Attempt must run no later than the requested date in Vietnam"));
    }
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM rooms WHERE id=?")
        .bind(r.room_id).fetch_optional(&state.db).await?;
    if exists.is_none() { return Err(ApiError::bad("Room is not loaded yet; refresh schedule")); }
    // A pre-booking is a future attempt, but do not allow one for a slot already
    // held locally (including an ambiguous upstream submission).
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM bookings WHERE room_id=? AND booking_date=? AND slot=? AND status IN ('pending','confirmed','uncertain')"
    ).bind(r.room_id).bind(&r.date).bind(r.slot).fetch_one(&state.db).await?;
    if active > 0 { return Err(ApiError::conflict("This room/shift is already booked or awaiting confirmation")); }

    let inserted = sqlx::query(
        "INSERT INTO scheduled_jobs(user_id,room_id,booking_date,slot,students,note,borrow_tv,run_at,kind,opens_at,retry_until) VALUES(?,?,?,?,?,?,?,?,?,?,?)"
    )
        .bind(user.id).bind(r.room_id).bind(&r.date).bind(r.slot)
        .bind(r.students).bind(r.note.trim()).bind(r.borrow_tv)
        .bind(run_at).bind(kind).bind(opens_at).bind(retry_until)
        .execute(&state.db).await;
    let id = match inserted {
        Ok(row) => row.last_insert_rowid(),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(ApiError::conflict("You already have an active scheduled attempt for this room/date/shift"));
        }
        Err(e) => return Err(e.into()),
    };
    // A new job may be earlier than the one the scheduler is sleeping for.
    state.scheduler_notify.notify_one();
    Ok((StatusCode::CREATED, Json(json!({
        "id": id, "status": "scheduled", "kind": kind,
        "run_at": run_at, "opens_at": opens_at,
        "retry_until": retry_until,
    }))))
}

pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Vec<Job>>, ApiError> {
    let user = auth::authenticate(&state, &headers).await?;
    let rows = sqlx::query("SELECT j.*,r.name AS room_name FROM scheduled_jobs j JOIN rooms r ON j.room_id=r.id WHERE j.user_id=? ORDER BY j.created_at DESC,j.id DESC LIMIT 100")
        .bind(user.id).fetch_all(&state.db).await?;
    let result = rows.into_iter().map(|r| Job {
        id: r.get("id"), room_id: r.get("room_id"), room_name: r.get("room_name"),
        date: r.get("booking_date"), slot: r.get("slot"), students: r.get("students"),
        note: r.get("note"), borrow_tv: r.get::<i64,_>("borrow_tv") != 0,
        run_at: r.get("run_at"), kind: r.get("kind"), opens_at: r.get("opens_at"),
        retry_until: r.get("retry_until"), attempts: r.get("attempts"),
        recovery_count: r.get("recovery_count"), recovered_at: r.get("recovered_at"),
        execution_phase: r.get("execution_phase"),
        status: r.get("status"), message: r.get("message"), booking_id: r.get("booking_id"),
    }).collect();
    Ok(Json(result))
}

pub async fn cancel(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    auth::check_origin(&headers, &state)?;
    let user = auth::authenticate(&state, &headers).await?;
    let changed = sqlx::query("UPDATE scheduled_jobs SET status='cancelled',message='Cancelled by user' WHERE id=? AND user_id=? AND status='scheduled'")
        .bind(id).bind(user.id).execute(&state.db).await?;
    if changed.rows_affected() == 0 { return Err(ApiError::conflict("Job is no longer cancellable")); }
    state.scheduler_notify.notify_one();
    Ok(Json(json!({"ok":true})))
}

// On startup, scheduled jobs are already durable: their stored run_at timestamps
// remain authoritative. Only work that was interrupted *while processing* needs
// special treatment. The phase is recorded before any possible upstream POST.
pub async fn recover_processing(state: &AppState) -> Result<(), ApiError> {
    let rows = sqlx::query("SELECT id,user_id,execution_phase,booking_id FROM scheduled_jobs WHERE status='processing' ORDER BY id")
        .fetch_all(&state.db).await?;
    let now = Utc::now().timestamp();
    for row in rows {
        let id: i64 = row.get("id");
        let user_id: i64 = row.get("user_id");
        let phase: String = row.get("execution_phase");
        let booking_id: Option<i64> = row.get("booking_id");
        if phase == "preflight" && booking_id.is_none() {
            // No local booking was inserted and no CTU submission started.
            // Requeue immediately, including jobs whose original run_at has passed.
            sqlx::query("UPDATE scheduled_jobs SET status='scheduled',run_at=MIN(run_at,?),recovery_count=recovery_count+1,recovered_at=?,message='Recovered after restart; no CTU submission had started' WHERE id=? AND status='processing'")
                .bind(now).bind(now).bind(id).execute(&state.db).await?;
            tracing::info!(job_id=id, "Recovered safely: preflight job scheduled again");
        } else if let Some(bid) = booking_id {
            reconcile_linked_booking(state, id, user_id, bid, true).await?;
        } else {
            // Legacy processing jobs predate phase tracking. We cannot determine
            // whether a POST went out. Do not duplicate the submission.
            sqlx::query("UPDATE scheduled_jobs SET status='uncertain',execution_phase='completed',recovery_count=recovery_count+1,recovered_at=?,finished_at=?,message='Interrupted legacy/ambiguous attempt; verify on CTU before trying again' WHERE id=? AND status='processing'")
                .bind(now).bind(now).bind(id).execute(&state.db).await?;
            tracing::warn!(job_id=id, "Ambiguous legacy job requires verification");
        }
    }
    // An interactive booking could also have been interrupted; a 'pending'
    // booking is NEVER interpreted as a safe reason to submit again.
    let pending = sqlx::query("SELECT id,user_id FROM bookings WHERE status='pending'")
        .fetch_all(&state.db).await?;
    for row in pending {
        let id: i64 = row.get("id");
        let uid: i64 = row.get("user_id");
        let message = "Server restarted during a possible CTU submission; check the official booking list";
        let changed = sqlx::query("UPDATE bookings SET status='uncertain',message=?,completed_at=? WHERE id=? AND status='pending'")
            .bind(message).bind(now).bind(id).execute(&state.db).await?;
        if changed.rows_affected() == 1 {
            let _ = telegram::enqueue(state,id,uid,format!("⚠️ CTU Library booking #{id}: {message}")).await;
        }
    }
    Ok(())
}

// A linked booking was written atomically with execution_phase='submission_started'.
// It may be confirmed, failed or uncertain even if the job status was not saved
// before a crash. A pending linked booking must become uncertain, never retried.
async fn reconcile_linked_booking(
    state: &AppState, job_id: i64, user_id: i64, booking_id: i64, recovered: bool,
) -> Result<(), ApiError> {
    let now = Utc::now().timestamp();
    let mut tx = state.db.begin().await?;
    let booking = sqlx::query("SELECT status,message FROM bookings WHERE id=? AND user_id=?")
        .bind(booking_id).bind(user_id).fetch_optional(&mut *tx).await?;
    let (status, message) = if let Some(row) = booking {
        let original: String = row.get("status");
        if original == "pending" {
            let msg = "The server stopped during a possible CTU submission; verify on the official site before retrying".to_string();
            sqlx::query("UPDATE bookings SET status='uncertain',message=?,completed_at=? WHERE id=? AND status='pending'")
                .bind(&msg).bind(now).bind(booking_id).execute(&mut *tx).await?;
            ("uncertain".to_string(),msg)
        } else {
            (original,row.get::<String,_>("message"))
        }
    } else {
        // Absent local data is anomalous, not an authorization for a second POST.
        ("uncertain".to_string(),"Linked booking row is missing; verify manually on CTU".to_string())
    };
    sqlx::query("UPDATE scheduled_jobs SET status=?,message=?,execution_phase='completed',finished_at=?,recovery_count=recovery_count+?,recovered_at=CASE WHEN ?=1 THEN ? ELSE recovered_at END WHERE id=? AND status='processing'")
        .bind(&status).bind(&message).bind(now).bind(if recovered { 1i64 } else { 0i64 })
        .bind(if recovered { 1i64 } else { 0i64 }).bind(now).bind(job_id).execute(&mut *tx).await?;
    tx.commit().await?;
    if recovered {
        let marker = match status.as_str() {
            "confirmed" => "✅", "failed" => "❌", _ => "⚠️"
        };
        if let Err(e) = telegram::enqueue(state,booking_id,user_id,
            format!("{marker} ITClub-AutoRoom job #{job_id}: {status}. {message}")).await {
            tracing::warn!(job_id,error=%e.1,"Recovered job notification was not queued");
        }
    }
    tracing::info!(job_id,booking_id,phase=?status,"Reconciled interrupted booking without a duplicate POST");
    Ok(())
}

// A temporary preflight failure can be rechecked during the opening window.
// Never requeue once a booking row exists or submission_started was recorded.
async fn postpone(state: &AppState, id: i64, deadline: i64, reason: &str) -> Result<bool, ApiError> {
    let now = Utc::now().timestamp();
    if now + RETRY_INTERVAL_SECONDS <= deadline {
        let result = sqlx::query("UPDATE scheduled_jobs SET status='scheduled',execution_phase='preflight',run_at=?,message=? WHERE id=? AND status='processing' AND execution_phase='preflight' AND booking_id IS NULL")
            .bind(now + RETRY_INTERVAL_SECONDS).bind(reason).bind(id).execute(&state.db).await?;
        if result.rows_affected() == 1 {
            state.scheduler_notify.notify_one();
            return Ok(true);
        }
        return Ok(false);
    }
    Ok(false)
}

struct DueJob {
    id: i64,
    user_id: i64,
    request: BookRequest,
    kind: String,
    retry_until: Option<i64>,
}

async fn process_claimed(state: AppState, job: DueJob) {
    let id = job.id;
    let user_id = job.user_id;
    let req = job.request;
    // Read-only preflight; no job should call CTU's booking endpoint until the
    // schedule explicitly reports the requested shift as bookable.
    if job.kind == "prebook" {
        match schedule::refresh(&state).await {
            Ok(calendar) => {
                let availability = calendar.slots.iter().find(|s|
                    s.room_id == req.room_id && s.date == req.date && s.slot == req.slot)
                    .map(|s| s.status.as_str());
                match availability {
                    Some("available") => {},
                    Some("reserved") | Some("closed") => {
                        finish_failure(&state,id,user_id,"CTU reports this shift as reserved or closed").await;
                        return;
                    }
                    _ => {
                        let reason = "Waiting for CTU to open this slot (no booking POST sent)";
                        if let Some(until) = job.retry_until {
                            if matches!(postpone(&state,id,until,reason).await,Ok(true)) {return;}
                        }
                        finish_failure(&state,id,user_id,"CTU did not open this shift during the checking window").await;
                        return;
                    }
                }
            }
            Err(_) => {
                if let Some(until) = job.retry_until {
                    if matches!(postpone(&state,id,until,"Schedule unavailable; no POST sent").await,Ok(true)) {return;}
                }
                finish_failure(&state,id,user_id,"CTU schedule was unavailable during the checking window").await;
                return;
            }
        }
    }

    let result = match auth::user_by_id(&state,user_id).await {
        Ok(user) => api::submit_for_user_with_job(&state,user,req,Some(id)).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(booking) => {
            if let Err(e) = sqlx::query("UPDATE scheduled_jobs SET status=?,message=?,booking_id=?,execution_phase='completed',finished_at=? WHERE id=? AND status='processing'")
                .bind(&booking.status).bind(&booking.message).bind(booking.id)
                .bind(Utc::now().timestamp()).bind(id).execute(&state.db).await {
                tracing::error!(job_id=id,error=%e,"Booking completed but job status update failed; startup reconciliation will fix it");
            }
        }
        Err(e) => {
            // An exception after inserting a booking row is not a failed attempt:
            // the CTU request may already have been sent. Inspect the durable link.
            let linked: Result<Option<i64>,sqlx::Error> = sqlx::query_scalar("SELECT booking_id FROM scheduled_jobs WHERE id=?")
                .bind(id).fetch_one(&state.db).await;
            match linked {
                Ok(Some(booking_id)) => {
                    if let Err(reconcile_err) = reconcile_linked_booking(&state,id,user_id,booking_id,false).await {
                        tracing::error!(job_id=id,error=%reconcile_err.1,"Could not reconcile linked booking");
                    }
                }
                Ok(None) => {
                    // The failure was before CTU submission. Only temporary
                    // service errors are eligible for the limited prebook retry.
                    if e.0 == StatusCode::SERVICE_UNAVAILABLE {
                        if let Some(until) = job.retry_until {
                            if matches!(postpone(&state,id,until,"CTU temporarily unavailable; no POST sent").await,Ok(true)) {return;}
                        }
                    }
                    finish_failure(&state,id,user_id,&e.1).await;
                }
                Err(db_err) => {
                    tracing::error!(job_id=id,error=%db_err,"Database unavailable while checking CTU submission state");
                    // Deliberately leave 'processing': startup will recover it.
                    // The watchdog/health endpoint will flag DB outage.
                }
            }
        }
    }
}

async fn finish_failure(state: &AppState, id: i64, user_id: i64, message: &str) {
    let saved = sqlx::query("UPDATE scheduled_jobs SET status='failed',execution_phase='completed',finished_at=?,message=? WHERE id=? AND status='processing' AND booking_id IS NULL")
        .bind(Utc::now().timestamp()).bind(message).bind(id).execute(&state.db).await;
    match saved {
        Ok(result) if result.rows_affected()==1 => {
            telegram::notify_job_failure(state,user_id,format!("❌ ITClub-AutoRoom scheduled booking #{id} failed: {message}")).await;
        }
        Ok(_) => tracing::warn!(job_id=id,"Job changed state before recording preflight failure"),
        Err(e) => tracing::error!(job_id=id,error=%e,"Unable to persist job failure"),
    }
}

/// Scan is intentionally separate from execution. One stalled CTU request cannot
/// hold up the scan of other due jobs or the liveness heartbeat.
async fn claim_due(state: &AppState, workers: &mut JoinSet<()>) -> Result<(), ApiError> {
    let capacity = MAX_CONCURRENT_JOBS.saturating_sub(workers.len());
    if capacity == 0 {
        // Keep checking database health even while all worker permits are busy.
        let _: i64 = sqlx::query_scalar("SELECT 1").fetch_one(&state.db).await?;
        state.scheduler_last_db_ok.store(Utc::now().timestamp(), Ordering::Relaxed);
        return Ok(());
    }
    let rows = sqlx::query("SELECT id,user_id,room_id,booking_date,slot,students,note,borrow_tv,kind,retry_until FROM scheduled_jobs WHERE status='scheduled' AND run_at<=? ORDER BY run_at,id LIMIT ?")
        .bind(Utc::now().timestamp()).bind(capacity as i64).fetch_all(&state.db).await?;
    state.scheduler_last_db_ok.store(Utc::now().timestamp(), Ordering::Relaxed);
    for row in rows {
        let id: i64 = row.get("id");
        let changed = sqlx::query("UPDATE scheduled_jobs SET status='processing',execution_phase='preflight',started_at=?,attempts=attempts+1 WHERE id=? AND status='scheduled' AND run_at<=?")
            .bind(Utc::now().timestamp()).bind(id).bind(Utc::now().timestamp()).execute(&state.db).await?;
        if changed.rows_affected()!=1 {continue;}
        let job = DueJob {
            id,
            user_id:row.get("user_id"),
            request:BookRequest {
                room_id:row.get("room_id"),date:row.get("booking_date"),slot:row.get("slot"),
                students:row.get("students"),note:row.get("note"),
                borrow_tv:row.get::<i64,_>("borrow_tv") != 0,
            },
            kind:row.get("kind"),retry_until:row.get("retry_until"),
        };
        tracing::info!(job_id=id,"Claimed persisted scheduled booking");
        let worker_state = state.clone();
        workers.spawn(async move {
            process_claimed(worker_state.clone(),job).await;
            // If a DB failure prevented final state persistence, force startup
            // recovery rather than leaving this job invisible forever.
            let state_check: Result<String,sqlx::Error> =
                sqlx::query_scalar("SELECT status FROM scheduled_jobs WHERE id=?")
                    .bind(id).fetch_one(&worker_state.db).await;
            match state_check {
                Ok(status) if status != "processing" => {},
                other => {
                    tracing::error!(job_id=id,db_state=?other,
                        "Scheduled worker exited without saving a terminal/requeue state");
                    std::process::exit(1);
                }
            }
        });
    }
    Ok(())
}

// Reads the earliest pending deadline. The indexed query is only performed on
// startup, after a queue change, after work completes, or on the watchdog tick.
async fn next_due_at(state: &AppState) -> Result<Option<i64>, ApiError> {
    let deadline: Option<i64> = sqlx::query_scalar(
        "SELECT MIN(run_at) FROM scheduled_jobs WHERE status='scheduled'"
    ).fetch_one(&state.db).await?;
    state.scheduler_last_db_ok.store(Utc::now().timestamp(), Ordering::Relaxed);
    Ok(deadline)
}

// Convert the stored wall-clock timestamp to a Tokio monotonic deadline. A
// periodic wake also handles VPS clock adjustments while the process is alive.
fn deadline_instant(unix_seconds: i64) -> Instant {
    let remaining_ms = unix_seconds.saturating_mul(1000)
        .saturating_sub(Utc::now().timestamp_millis()).max(100);
    Instant::now() + TokioDuration::from_millis(remaining_ms as u64)
}

fn handle_worker_result(joined: Result<(), tokio::task::JoinError>) {
    if let Err(err) = joined {
        // Do not keep running with an unexamined processing job after a panic.
        tracing::error!(error=%err, "Scheduled worker panicked; restarting for safe recovery");
        std::process::exit(1);
    }
}

/// Event-driven scheduler: sleeps until the next job, immediately wakes when
/// the API updates the queue or a worker finishes, and periodically verifies
/// database health. Startup has already reconciled interrupted submissions.
pub async fn loop_jobs(state: AppState) {
    let mut watchdog = tokio::time::interval(TokioDuration::from_secs(WATCHDOG_SCAN_SECONDS));
    watchdog.set_missed_tick_behavior(MissedTickBehavior::Skip);
    watchdog.tick().await; // Consume the immediate first tick; next recovery check is in 30s.
    let mut workers = JoinSet::new();
    loop {
        state.scheduler_heartbeat.store(Utc::now().timestamp(), Ordering::Relaxed);
        while let Some(joined) = workers.try_join_next() {
            handle_worker_result(joined);
        }

        // Due jobs are claimed atomically and delegated to independent workers.
        // Nothing is repeated after a possible upstream booking submission.
        if let Err(error) = claim_due(&state, &mut workers).await {
            tracing::error!(error=%error.1, "Cannot claim jobs; backing off");
            tokio::select! {
                _ = tokio::time::sleep(TokioDuration::from_secs(ERROR_BACKOFF_SECONDS)) => {},
                _ = state.scheduler_notify.notified() => {},
                _ = watchdog.tick() => {},
            }
            continue;
        }

        // While all workers are busy, the next wake should come from a worker
        // completing (or the watchdog), NOT from a due deadline already passed.
        let deadline = if workers.len() < MAX_CONCURRENT_JOBS {
            match next_due_at(&state).await {
                Ok(next) => next.map(deadline_instant),
                Err(error) => {
                    tracing::error!(error=%error.1, "Cannot read next job deadline; backing off");
                    tokio::select! {
                        _ = tokio::time::sleep(TokioDuration::from_secs(ERROR_BACKOFF_SECONDS)) => {},
                        _ = state.scheduler_notify.notified() => {},
                        _ = watchdog.tick() => {},
                    }
                    continue;
                }
            }
        } else {
            None
        };

        // None means no pending jobs (or no free worker slots). In that case
        // this future is inert, so only Notify, worker completion or the
        // watchdog can wake the loop. No 2-second database polling occurs.
        let deadline_wait = async {
            if let Some(at) = deadline {
                tokio::time::sleep_until(at).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            _ = deadline_wait => {},
            _ = state.scheduler_notify.notified() => {},
            joined = workers.join_next(), if !workers.is_empty() => {
                if let Some(result) = joined { handle_worker_result(result); }
            },
            _ = watchdog.tick() => {},
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interrupted_jobs_are_recovered_without_replaying_possible_ctu_posts() {
        use crate::{config::Config, state::AppState};
        use sqlx::sqlite::SqlitePoolOptions;
        use std::{sync::{Arc,atomic::AtomicI64},time::Duration};
        use tokio::sync::Mutex;
        let db = SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        sqlx::query("INSERT INTO users(id,username,password_hash,ctu_username,ctu_password_ciphertext) VALUES(1,'tester','hash','ctu','cipher')")
            .execute(&db).await.unwrap();
        sqlx::query("INSERT INTO rooms(id,name,floor) VALUES(1,'A','2')").execute(&db).await.unwrap();
        let now = Utc::now().timestamp();
        for id in 1..=4 {
            sqlx::query("INSERT INTO scheduled_jobs(id,user_id,room_id,booking_date,slot,students,note,run_at,status,execution_phase) VALUES(?,1,1,'2026-12-02',?,5,'Test',?,'processing',?)")
                .bind(id).bind(id).bind(now-10)
                .bind(if id == 1 {"preflight"} else if id == 4 {"legacy_unknown"} else {"submission_started"})
                .execute(&db).await.unwrap();
        }
        for (id, status) in [(2,"pending"),(3,"confirmed")] {
            let inserted = sqlx::query("INSERT INTO bookings(user_id,room_id,booking_date,slot,students,note,status,message) VALUES(1,1,'2026-12-02',?,5,'Test',?,'Saved CTU result')")
                .bind(id).bind(status).execute(&db).await.unwrap();
            sqlx::query("UPDATE scheduled_jobs SET booking_id=? WHERE id=?")
                .bind(inserted.last_insert_rowid()).bind(id).execute(&db).await.unwrap();
        }
        let cfg = Config {
            bind:"127.0.0.1:0".into(), database_url:"sqlite::memory:".into(),
            encryption_key:[0;32], ctu_base:"https://libapp.ctu.edu.vn/dkptl".into(),
            allowed_origins:vec![],cookie_secure:false,telegram_token:None,telegram_username:None,
            refresh_secs:120,seed_fixture:false,
        };
        let state=AppState {
            db:db.clone(),http:reqwest::Client::builder().timeout(Duration::from_secs(2)).build().unwrap(),
            cfg:Arc::new(cfg),booking_gate:Arc::new(Mutex::new(())),sync_gate:Arc::new(Mutex::new(())),
            scheduler_heartbeat:Arc::new(AtomicI64::new(now)),scheduler_last_db_ok:Arc::new(AtomicI64::new(now)),scheduler_notify:Arc::new(tokio::sync::Notify::new()),
        };
        recover_processing(&state).await.unwrap();
        let rows=sqlx::query("SELECT id,status,recovery_count FROM scheduled_jobs ORDER BY id")
            .fetch_all(&db).await.unwrap();
        assert_eq!(rows[0].get::<String,_>("status"), "scheduled");
        assert_eq!(rows[1].get::<String,_>("status"), "uncertain");
        assert_eq!(rows[2].get::<String,_>("status"), "confirmed");
        assert_eq!(rows[3].get::<String,_>("status"), "uncertain");
        for row in &rows { assert_eq!(row.get::<i64,_>("recovery_count"),1); }
        let booking_status:String=sqlx::query_scalar("SELECT status FROM bookings WHERE slot=2")
            .fetch_one(&db).await.unwrap();
        assert_eq!(booking_status,"uncertain");
        // Idempotent subsequent startup: no further recovery or CTU submission.
        recover_processing(&state).await.unwrap();
        let count:i64=sqlx::query_scalar("SELECT SUM(recovery_count) FROM scheduled_jobs")
            .fetch_one(&db).await.unwrap();
        assert_eq!(count,4);
    }

    #[test]
    fn event_deadline_is_immediate_for_overdue_and_in_future_for_pending() {
        let overdue = deadline_instant(Utc::now().timestamp() - 60);
        assert!(overdue <= Instant::now() + TokioDuration::from_secs(1));
        let pending = deadline_instant(Utc::now().timestamp() + 120);
        let left = pending.duration_since(Instant::now());
        assert!(left > TokioDuration::from_secs(118));
        assert!(left <= TokioDuration::from_secs(121));
    }

    #[tokio::test]
    async fn notify_wakes_an_idle_scheduler() {
        let signal = tokio::sync::Notify::new();
        let wait = signal.notified();
        signal.notify_one();
        tokio::time::timeout(TokioDuration::from_millis(250), wait)
            .await.expect("change notification must wake scheduler");
    }

    #[test]
    fn prebooking_thirteen_days_earlier_across_month() {
        let day = NaiveDate::from_ymd_opt(2026, 12, 2).unwrap();
        let at = prebook_open_timestamp(day, "16:20").unwrap();
        let utc = DateTime::<Utc>::from_timestamp(at, 0).unwrap();
        assert_eq!(utc.format("%Y-%m-%d %H:%M").to_string(), "2026-11-19 09:20");
    }

    #[test]
    fn prebooking_across_new_year_and_invalid_time() {
        let day = NaiveDate::from_ymd_opt(2027, 1, 5).unwrap();
        let at = prebook_open_timestamp(day, "00:00").unwrap();
        let utc = DateTime::<Utc>::from_timestamp(at, 0).unwrap();
        assert_eq!(utc.format("%Y-%m-%d %H:%M").to_string(), "2026-12-22 17:00");
        assert!(prebook_open_timestamp(day, "25:00").is_err());
    }
}
