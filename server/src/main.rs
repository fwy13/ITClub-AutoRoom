mod api;
mod auth;
mod config;
mod crypto;
mod ctu;
mod error;
mod jobs;
mod schedule;
mod state;
mod telegram;
mod window;

use axum::{routing::{get,post}, Router};
use config::Config;
use reqwest::Client;
use sqlx::{sqlite::{SqliteConnectOptions,SqliteJournalMode,SqlitePoolOptions},migrate::Migrator};
use state::AppState;
use std::{str::FromStr,sync::{Arc,atomic::{AtomicI64,Ordering}},time::Duration};
use chrono::Utc;
use tokio::sync::{Mutex, Notify};
use tower_http::{services::{ServeDir,ServeFile},trace::TraceLayer};
use tracing_subscriber::EnvFilter;

static MIGRATOR: Migrator=sqlx::migrate!();

#[tokio::main]
async fn main()->Result<(),Box<dyn std::error::Error>>{
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_|EnvFilter::new("info,tower_http=warn"))).init();
    let cfg=Config::from_env().map_err(std::io::Error::other)?;
    let options=SqliteConnectOptions::from_str(&cfg.database_url)?
        .create_if_missing(true).journal_mode(SqliteJournalMode::Wal).foreign_keys(true).busy_timeout(Duration::from_secs(8));
    let db=SqlitePoolOptions::new().max_connections(5).connect_with(options).await?;
    MIGRATOR.run(&db).await?;
    let http=Client::builder().timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("ITClub-AutoRoom/0.2 (+authorized student bookings)").build()?;
    let now=Utc::now().timestamp();
    let state=AppState{db,http,cfg:Arc::clone(&cfg),booking_gate:Arc::new(Mutex::new(())),sync_gate:Arc::new(Mutex::new(())),scheduler_heartbeat:Arc::new(AtomicI64::new(now)),scheduler_last_db_ok:Arc::new(AtomicI64::new(now)),scheduler_notify:Arc::new(Notify::new())};
    schedule::maybe_seed(&state).await.map_err(|_|std::io::Error::other("fixture seed failed"))?;
    jobs::recover_processing(&state).await.map_err(|_|std::io::Error::other("job recovery failed"))?;
    let scheduler_handle = tokio::spawn(jobs::loop_jobs(state.clone()));
    // A failed/panicked scheduler must not leave the web UI looking healthy.
    tokio::spawn(async move {
        let result = scheduler_handle.await;
        tracing::error!(result=?result,"Scheduler stopped unexpectedly; letting Docker restart the app");
        std::process::exit(1);
    });
    let monitor = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let now = Utc::now().timestamp();
            let heartbeat = monitor.scheduler_heartbeat.load(Ordering::Relaxed);
            let db_ok = monitor.scheduler_last_db_ok.load(Ordering::Relaxed);
            if now - heartbeat > 90 || now - db_ok > 150 {
                tracing::error!(heartbeat_age=now-heartbeat,db_scan_age=now-db_ok,
                    "Scheduler stuck or database scans failing; restarting process");
                std::process::exit(1);
            }
        }
    });
    let background=state.clone();
    tokio::spawn(async move {
        let mut interval=tokio::time::interval(Duration::from_secs(background.cfg.refresh_secs));
        loop {
            interval.tick().await;
            if let Err(e)=schedule::refresh(&background).await {tracing::warn!(error=%e.1,"background CTU schedule refresh failed");}
        }
    });
    tokio::spawn(telegram::poll_bot(state.clone()));
    tokio::spawn(telegram::notification_loop(state.clone()));
    // Remove expired sessions periodically.
    let housekeeper=state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            let _=sqlx::query("DELETE FROM sessions WHERE expires_at<unixepoch()").execute(&housekeeper.db).await;
        }
    });
    let api=Router::new()
        .route("/healthz",get(api::health))
        .route("/api/auth/register",post(auth::register))
        .route("/api/auth/login",post(auth::login))
        .route("/api/auth/logout",post(auth::logout))
        .route("/api/auth/me",get(auth::me))
        .route("/api/schedule",get(api::get_schedule))
        .route("/api/schedule/refresh",post(api::refresh_schedule))
        .route("/api/bookings",get(api::my_bookings).post(api::create_booking))
        .route("/api/bookings/{id}/cancel",post(api::cancel_booking))
        .route("/api/jobs",get(jobs::list).post(jobs::create))
        .route("/api/jobs/{id}",axum::routing::delete(jobs::cancel))
        .route("/api/telegram/link",post(telegram::create_link))
        .route("/api/telegram/unlink",post(telegram::unlink))
        .fallback_service(ServeDir::new("public").fallback(ServeFile::new("public/index.html")))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let listener=tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(bind=%cfg.bind,"ITClub-AutoRoom ready");
    axum::serve(listener,api).await?;
    Ok(())
}
