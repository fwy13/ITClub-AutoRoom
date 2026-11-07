use std::{env, sync::Arc};

#[derive(Clone)]
pub struct Config {
    pub bind: String,
    pub database_url: String,
    pub encryption_key: [u8; 32],
    pub ctu_base: String,
    pub allowed_origins: Vec<String>,
    pub cookie_secure: bool,
    pub telegram_token: Option<String>,
    pub telegram_username: Option<String>,
    pub refresh_secs: u64,
    pub seed_fixture: bool,
}
impl Config {
    pub fn from_env() -> Result<Arc<Self>, String> {
        let key_hex = env::var("APP_ENCRYPTION_KEY").map_err(|_| "APP_ENCRYPTION_KEY must be set (64 hexadecimal characters)")?;
        let bytes = hex::decode(key_hex).map_err(|_| "APP_ENCRYPTION_KEY must be hexadecimal")?;
        let encryption_key: [u8; 32] = bytes.try_into().map_err(|_| "APP_ENCRYPTION_KEY must be exactly 32 bytes")?;
        let ctu_base = env::var("CTU_BASE_URL").unwrap_or_else(|_| "https://libapp.ctu.edu.vn/dkptl".into());
        if ctu_base != "https://libapp.ctu.edu.vn/dkptl" && env::var("ALLOW_CUSTOM_CTU_URL").as_deref() != Ok("true") {
            return Err("Refusing nonstandard CTU_BASE_URL. Set ALLOW_CUSTOM_CTU_URL=true only for controlled testing".into());
        }
        Ok(Arc::new(Self {
            bind: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".into()),
            database_url: env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:///data/reservations.db".into()),
            encryption_key,
            ctu_base,
            allowed_origins: env::var("APP_ALLOWED_ORIGINS").unwrap_or_else(|_| "http://localhost:3000,http://localhost:5173".into()).split(',').map(|x| x.trim().trim_end_matches('/').to_owned()).filter(|x| !x.is_empty()).collect(),
            cookie_secure: env::var("COOKIE_SECURE").as_deref() != Ok("false"),
            telegram_token: env::var("TELEGRAM_BOT_TOKEN").ok().filter(|x| !x.is_empty()),
            telegram_username: env::var("TELEGRAM_BOT_USERNAME").ok().filter(|x| !x.is_empty()).map(|x| x.trim_start_matches('@').to_string()),
            refresh_secs: env::var("SCHEDULE_REFRESH_SECONDS").ok().and_then(|x| x.parse().ok()).unwrap_or(120).max(60),
            seed_fixture: env::var("SEED_SNAPSHOT").as_deref() == Ok("true"),
        }))
    }
}
