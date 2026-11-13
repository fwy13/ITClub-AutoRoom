use crate::{api::{self, BookRequest}, auth, crypto, ctu, error::ApiError, schedule, state::AppState, telegram, window};
use axum::{extract::{Path, State}, http::{HeaderMap, StatusCode}, Json};
use std::sync::atomic::Ordering;
use tokio::task::JoinSet;
use chrono::{NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use tokio::time::{Duration as TokioDuration, Instant, MissedTickBehavior};

// Start checking at the first eligible midnight; CTU's actual opening hour is
// not documented in the supplied source. Avoid assuming that midnight is open.
// Only pre-submission checks may retry. After a possible POST, never retry.
const RETRY_WINDOW_SECONDS: i64 = 24 * 3600 - 60;
const RETRY_INTERVAL_SECONDS: i64 = 5 * 60;
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

/// Legacy queued jobs can still be listed and cancelled, but no new jobs are accepted.
pub async fn create(
    State(state): State<AppState>, headers: HeaderMap, Json(_request): Json<NewJob>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth::check_origin(&headers, &state)?;
    let _ = auth::authenticate(&state, &headers).await?;
    Err(ApiError(StatusCode::GONE, "Scheduled booking has been removed; select an available shift to book immediately".into()))
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
    tracing::info!(stage="cancelled", job_id=id, "Scheduled booking cancelled by owner");
    state.scheduler_notify.notify_one();
    Ok(Json(json!({"ok":true})))
}

// On startup, scheduled jobs are already durable: their stored run_at timestamps
// remain authoritative. Only work that was interrupted *while processing* needs
// special treatment. The phase is recorded before any possible upstream POST.
pub async fn recover_processing(state: &AppState) -> Result<(), ApiError> {
    // An interrupted cancellation may have reached CTU. Never send it twice.
    let interrupted = sqlx::query("UPDATE bookings SET cancellation_status='uncertain',message='Server restarted during cancellation; verify on CTU before retrying' WHERE cancellation_status='processing'")
        .execute(&state.db).await?;
    if interrupted.rows_affected() > 0 {
        tracing::warn!(count=interrupted.rows_affected(), "Interrupted CTU cancellations marked uncertain");
    }
    let rows = sqlx::query("SELECT id,user_id,execution_phase,booking_id FROM scheduled_jobs WHERE status='processing' ORDER BY id")
        .fetch_all(&state.db).await?;
    let now = Utc::now().timestamp();
    tracing::info!(stage="startup_recovery", interrupted_jobs=rows.len(),
        "Checking interrupted scheduled jobs after restart");
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
            tracing::info!(stage="startup_recovery", job_id=id, prior_phase=%phase,
                outcome="requeued", "Recovered pre-submission job safely");
        } else if let Some(bid) = booking_id {
            reconcile_linked_booking(state, id, user_id, bid, true).await?;
        } else {
            // Legacy processing jobs predate phase tracking. We cannot determine
            // whether a POST went out. Do not duplicate the submission.
            sqlx::query("UPDATE scheduled_jobs SET status='uncertain',execution_phase='completed',recovery_count=recovery_count+1,recovered_at=?,finished_at=?,message='Interrupted legacy/ambiguous attempt; verify on CTU before trying again' WHERE id=? AND status='processing'")
                .bind(now).bind(now).bind(id).execute(&state.db).await?;
            tracing::warn!(stage="startup_recovery", job_id=id, prior_phase=%phase,
                outcome="uncertain", "Legacy job interrupted: CTU submission must be verified");
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
fn next_retry_at(now: i64, deadline: i64) -> Option<i64> {
    // Never schedule after the window; still allow one final check at its end.
    (now < deadline).then_some(now.saturating_add(RETRY_INTERVAL_SECONDS).min(deadline))
}

async fn postpone(state: &AppState, id: i64, deadline: i64, reason: &str) -> Result<bool, ApiError> {
    let now = Utc::now().timestamp();
    if let Some(next) = next_retry_at(now, deadline) {
        let result = sqlx::query("UPDATE scheduled_jobs SET status='scheduled',execution_phase='preflight',run_at=?,message=? WHERE id=? AND status='processing' AND execution_phase='preflight' AND booking_id IS NULL")
            .bind(next).bind(reason).bind(id).execute(&state.db).await?;
        if result.rows_affected() == 1 {
            tracing::info!(stage="preflight_retry", job_id=id, next_run_at=next,
                retry_deadline=deadline, reason, "CTU slot not ready; retry scheduled without POST");
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
    let started = Instant::now();
    let id = job.id;
    let user_id = job.user_id;
    let req = job.request;
    tracing::info!(stage="preflight", job_id=id, kind=%job.kind,
        room_id=req.room_id, booking_date=%req.date, shift=req.slot,
        "Scheduled booking worker started");
    // Read-only preflight. A slot can be vacant without being clickable in an
    // anonymous CTU timetable. The authenticated CTU flow rechecks occupancy
    // and CTU decides whether a submission is accepted.
    if job.kind == "prebook" {
        match schedule::refresh(&state).await {
            Ok(calendar) => {
                let availability = calendar.slots.iter().find(|s|
                    s.room_id == req.room_id && s.date == req.date && s.slot == req.slot)
                    .map(|s| s.status.as_str());
                tracing::info!(stage="preflight", job_id=id, room_id=req.room_id,
                    booking_date=%req.date, shift=req.slot, slot_status=?availability,
                    "CTU timetable preflight occupancy check");
                match availability {
                    Some("available") | Some("vacant") => {},
                    Some("reserved") | Some("closed") => {
                        finish_failure(&state,id,user_id,"CTU reports this shift as reserved or closed").await;
                        return;
                    }
                    // `&str` is an open type: do not enumerate Some(_) variants.
                    // Unknown, expired or missing cells are NOT permission to POST.
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
            Err(error) => {
                tracing::warn!(stage="preflight", job_id=id, error=%error.1,
                    "Live CTU timetable cannot be checked");
                if let Some(until) = job.retry_until {
                    if matches!(postpone(&state,id,until,"Schedule unavailable; no POST sent").await,Ok(true)) {return;}
                }
                finish_failure(&state,id,user_id,"CTU schedule was unavailable during the checking window").await;
                return;
            }
        }
    }

    // The anonymous calendar can say `vacant` even when CTU is NOT accepting
    // reservations yet. Check the authenticated calendar BEFORE inserting a
    // local booking or setting submission_started. Only a visibly open slot
    // may cause the one-shot CTU booking POST.
    if job.kind == "prebook" {
        let user = match auth::user_by_id(&state,user_id).await {
            Ok(user) => user,
            Err(error) => {
                finish_failure(&state,id,user_id,&error.1).await;
                return;
            }
        };
        let password = match crypto::decrypt(&state.cfg.encryption_key, &user.ctu_password_ciphertext) {
            Ok(password) => password,
            Err(_) => {
                finish_failure(&state,id,user_id,"Cannot decrypt CTU credentials; no booking POST sent").await;
                return;
            }
        };
        match ctu::preflight_authenticated(&state, &user.ctu_username, &password,
            req.room_id, req.slot, &req.date).await {
            ctu::PreflightOutcome::Ready => {},
            ctu::PreflightOutcome::Blocked(reason) => {
                finish_failure(&state,id,user_id,&reason).await;
                return;
            }
            ctu::PreflightOutcome::Waiting(reason) => {
                if let Some(until) = job.retry_until {
                    if matches!(postpone(&state,id,until,&reason).await,Ok(true)) {return;}
                }
                finish_failure(&state,id,user_id,&format!(
                    "CTU booking window elapsed without an authenticated open slot: {reason}"
                )).await;
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
                tracing::error!(stage="persist_result", job_id=id,booking_id=booking.id,error=%e,
                    "Booking completed but job status update failed; startup reconciliation will fix it");
            } else {
                tracing::info!(stage="completed", job_id=id, booking_id=booking.id,
                    outcome=%booking.status, elapsed_ms=started.elapsed().as_millis() as u64,
                    "Scheduled booking result persisted");
            }
        }
        Err(e) => {
            tracing::warn!(stage="booking_error", job_id=id, http_status=e.0.as_u16(),
                error=%e.1, elapsed_ms=started.elapsed().as_millis() as u64,
                "Scheduled booking flow returned an error before or during submission; inspecting durable link");
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
            tracing::warn!(stage="completed", job_id=id, outcome="failed", reason=message,
                "Scheduled booking failed before CTU submission");
            telegram::notify_job_failure(state,user_id,format!("❌ ITClub-AutoRoom: Lịch đăng ký #{id} thất bại.\nChi tiết: {message}")).await;
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
    let rows = sqlx::query("SELECT id,user_id,room_id,booking_date,slot,students,note,borrow_tv,kind,retry_until,attempts,run_at FROM scheduled_jobs WHERE status='scheduled' AND run_at<=? ORDER BY run_at,id LIMIT ?")
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
        tracing::info!(stage="claimed", job_id=id, kind=%job.kind,
            room_id=job.request.room_id, booking_date=%job.request.date,
            shift=job.request.slot, attempt=row.get::<i64,_>("attempts") + 1,
            scheduled_at=row.get::<i64,_>("run_at"),
            "Claimed scheduled job from SQLite; starting worker");
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
    tracing::info!(stage="scheduler", watchdog_seconds=WATCHDOG_SCAN_SECONDS,
        max_parallel_jobs=MAX_CONCURRENT_JOBS,
        "Event-driven scheduler started; waiting for SQLite deadlines");
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
    use chrono::DateTime;

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
        sqlx::query("INSERT INTO bookings(user_id,room_id,booking_date,slot,students,note,status,cancellation_status) VALUES(1,1,'2026-12-02',5,5,'Cancel','confirmed','processing')")
            .execute(&db).await.unwrap();
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
        let cancel_state: String = sqlx::query_scalar("SELECT cancellation_status FROM bookings WHERE slot=5")
            .fetch_one(&db).await.unwrap();
        assert_eq!(cancel_state, "uncertain");
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
    fn retry_deadline_is_inclusive_and_does_not_overflow() {
        assert_eq!(next_retry_at(100, 100), None);
        assert_eq!(next_retry_at(100, 101), Some(101));
        assert_eq!(next_retry_at(100, 500), Some(400));
        assert_eq!(next_retry_at(i64::MAX - 2, i64::MAX), Some(i64::MAX));
    }

    #[tokio::test]
    async fn existing_user_booking_prevents_advance_overlap() {
        use crate::{config::Config, state::AppState};
        use sqlx::sqlite::SqlitePoolOptions;
        use std::{sync::{Arc, atomic::AtomicI64}, time::Duration};
        use tokio::sync::Mutex;
        let db = SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        sqlx::query("INSERT INTO users(id,username,password_hash,ctu_username,ctu_password_ciphertext) VALUES(1,'test','hash','ctu','cipher')")
            .execute(&db).await.unwrap();
        sqlx::query("INSERT INTO rooms(id,name,floor) VALUES(1,'Room A','2'),(2,'Room B','2')")
            .execute(&db).await.unwrap();
        let now = Utc::now().timestamp();
        let cfg = Config {
            bind:"127.0.0.1:0".into(), database_url:"sqlite::memory:".into(),
            encryption_key:[0;32], ctu_base:"http://127.0.0.1:1/dkptl".into(),
            allowed_origins:vec![],cookie_secure:false,telegram_token:None,telegram_username:None,
            refresh_secs:120,seed_fixture:false,
        };
        let state = AppState {
            db:db.clone(),http:reqwest::Client::builder().timeout(Duration::from_secs(2)).build().unwrap(),
            cfg:Arc::new(cfg),booking_gate:Arc::new(Mutex::new(())),sync_gate:Arc::new(Mutex::new(())),
            scheduler_heartbeat:Arc::new(AtomicI64::new(now)),scheduler_last_db_ok:Arc::new(AtomicI64::new(now)),
            scheduler_notify:Arc::new(tokio::sync::Notify::new()),
        };
        let target = window::today_vn(Utc::now()) + chrono::Duration::days(14);
        let target_str = target.format("%Y-%m-%d").to_string();
        sqlx::query("INSERT INTO bookings(user_id,room_id,booking_date,slot,students,note,status) VALUES(1,2,?,1,5,'Existing','confirmed')")
            .bind(&target_str).execute(&db).await.unwrap();
        let token = "a".repeat(64);
        sqlx::query("INSERT INTO sessions(token_hash,user_id,expires_at) VALUES(?,1,?)")
            .bind(crate::crypto::digest(&token)).bind(now+300).execute(&db).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::COOKIE, format!("ctu_session={token}").parse().unwrap());
        let new_job = || NewJob {
            request: BookRequest {
                room_id:1, date:target_str.clone(), slot:1,
                students:5, note:"Test".into(), borrow_tv:false,
            },
            kind:Some("prebook".into()), run_at:None, open_time_vn:None,
        };
        // The other reservation is in Room B; only the user-overlap rule rejects it.
        let error = create(State(state.clone()), headers.clone(), Json(new_job()))
            .await.err().expect("overlapping active reservation must be rejected");
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert!(error.1.contains("overlapping active"));
        // An explicitly cancelled CTU booking must no longer block a new attempt.
        sqlx::query("UPDATE bookings SET cancellation_status='cancelled' WHERE user_id=1")
            .execute(&db).await.unwrap();
        let (_status, body) = create(State(state), headers, Json(new_job())).await.unwrap();
        assert_eq!(body.0["status"], "scheduled");
    }

    #[test]
    fn advance_window_starts_thirteen_days_prior() {
        let day = NaiveDate::from_ymd_opt(2026, 12, 2).unwrap();
        let at = window::starts_at(day).unwrap();
        let utc = DateTime::<Utc>::from_timestamp(at, 0).unwrap();
        assert_eq!(utc.format("%Y-%m-%d %H:%M").to_string(), "2026-11-18 17:00");
    }
}
