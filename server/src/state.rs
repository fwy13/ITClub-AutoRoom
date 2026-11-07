use crate::config::Config;
use reqwest::Client;
use sqlx::SqlitePool;
use std::sync::{Arc,atomic::AtomicI64};
use tokio::sync::{Mutex, Notify};

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub http: Client,
    pub cfg: Arc<Config>,
    pub booking_gate: Arc<Mutex<()>>,
    pub sync_gate: Arc<Mutex<()>>,
    pub scheduler_heartbeat: Arc<AtomicI64>,
    pub scheduler_last_db_ok: Arc<AtomicI64>,
    /// Wakes the single scheduler immediately when the job queue changes.
    pub scheduler_notify: Arc<Notify>,
}
