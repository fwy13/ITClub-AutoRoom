use crate::{crypto, error::ApiError, state::AppState};
use argon2::{Argon2, PasswordHasher, PasswordVerifier, password_hash::{PasswordHash,SaltString,rand_core::OsRng}};
use axum::{extract::State, http::{header::{COOKIE, ORIGIN, SET_COOKIE}, HeaderMap, HeaderValue, StatusCode}, Json};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::Row;

const SESSION_SECONDS: i64 = 60 * 60 * 24 * 7;

#[derive(Debug,Clone)]
pub struct User {
    pub id:i64,
    pub username:String,
    pub ctu_username:String,
    pub ctu_password_ciphertext:String,
    pub telegram_chat_id: Option<i64>,
    pub telegram_username: Option<String>,
}
#[derive(Serialize)]
pub struct PublicUser {
    id:i64, username:String,ctu_username:String,telegram_linked:bool,telegram_username:Option<String>
}
impl From<User> for PublicUser {
    fn from(u:User)->Self { Self{id:u.id,username:u.username,ctu_username:u.ctu_username,telegram_linked:u.telegram_chat_id.is_some(),telegram_username:u.telegram_username} }
}

pub fn check_origin(headers:&HeaderMap, state:&AppState)->Result<(),ApiError> {
    if let Some(value)=headers.get(ORIGIN) {
        let origin=value.to_str().map_err(|_| ApiError::bad("Invalid Origin"))?.trim_end_matches('/');
        if !state.cfg.allowed_origins.iter().any(|v|v==origin) {
            return Err(ApiError(StatusCode::FORBIDDEN,"Origin not allowed".into()));
        }
    }
    if headers.get("sec-fetch-site").and_then(|v|v.to_str().ok()) == Some("cross-site") {
        return Err(ApiError(StatusCode::FORBIDDEN,"Cross-site requests are blocked".into()));
    }
    Ok(())
}


pub async fn user_by_id(state:&AppState, id:i64)->Result<User,ApiError> {
    let row=sqlx::query("SELECT id,username,ctu_username,ctu_password_ciphertext,telegram_chat_id,telegram_username FROM users WHERE id=?")
        .bind(id).fetch_optional(&state.db).await?.ok_or_else(ApiError::unauthorized)?;
    Ok(User{id:row.get("id"),username:row.get("username"),ctu_username:row.get("ctu_username"),ctu_password_ciphertext:row.get("ctu_password_ciphertext"),telegram_chat_id:row.get("telegram_chat_id"),telegram_username:row.get("telegram_username")})
}

pub async fn authenticate(state:&AppState, headers:&HeaderMap)->Result<User,ApiError> {
    let cookie=headers.get(COOKIE).and_then(|x| x.to_str().ok()).unwrap_or("");
    let token=cookie.split(';').filter_map(|chunk|chunk.trim().split_once('=')).find_map(|(k,v)|if k=="ctu_session"{Some(v)}else{None}).ok_or_else(ApiError::unauthorized)?;
    if token.len()!=64 || !token.bytes().all(|c|c.is_ascii_hexdigit()){return Err(ApiError::unauthorized());}
    let hash=crypto::digest(token);
    let row=sqlx::query("SELECT u.id,u.username,u.ctu_username,u.ctu_password_ciphertext,u.telegram_chat_id,u.telegram_username FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token_hash=? AND s.expires_at>?")
        .bind(hash).bind(Utc::now().timestamp()).fetch_optional(&state.db).await?.ok_or_else(ApiError::unauthorized)?;
    Ok(User{id:row.get("id"),username:row.get("username"),ctu_username:row.get("ctu_username"),ctu_password_ciphertext:row.get("ctu_password_ciphertext"),telegram_chat_id:row.get("telegram_chat_id"),telegram_username:row.get("telegram_username")})
}

fn cookie(value:&str, secure:bool, max_age:i64)->HeaderMap {
    let mut h=HeaderMap::new();
    let secure_attr=if secure{"; Secure"}else{""};
    let value=format!("ctu_session={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure_attr}");
    h.insert(SET_COOKIE,HeaderValue::from_str(&value).expect("safe cookie value"));
    h.insert("cache-control",HeaderValue::from_static("no-store"));
    h
}
async fn create_session(state:&AppState, id:i64)->Result<HeaderMap,ApiError> {
    let token=crypto::random_token();
    sqlx::query("INSERT INTO sessions (token_hash,user_id,expires_at) VALUES(?,?,?)")
        .bind(crypto::digest(&token)).bind(id).bind(Utc::now().timestamp()+SESSION_SECONDS).execute(&state.db).await?;
    Ok(cookie(&token,state.cfg.cookie_secure,SESSION_SECONDS))
}

#[derive(Deserialize)]
pub struct RegisterRequest {username:String,password:String,ctu_username:String,ctu_password:String}
fn valid_username(s:&str)->bool {
    (3..=32).contains(&s.len()) && s.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'_')
}
pub async fn register(State(state):State<AppState>, headers:HeaderMap, Json(payload):Json<RegisterRequest>) -> Result<(StatusCode,HeaderMap,Json<PublicUser>),ApiError> {
    check_origin(&headers,&state)?;
    if !valid_username(&payload.username){return Err(ApiError::bad("Username must have 3–32 ASCII letters, numbers or underscores"));}
    if !(10..=128).contains(&payload.password.len()){return Err(ApiError::bad("App password must have 10–128 characters"));}
    if !valid_username(&payload.ctu_username) || payload.ctu_password.is_empty() || payload.ctu_password.len()>256 {
        return Err(ApiError::bad("CTU username/password are invalid"));
    }
    let salt=SaltString::generate(&mut OsRng);
    let hash=Argon2::default().hash_password(payload.password.as_bytes(),&salt).map_err(|_|ApiError::internal())?.to_string();
    let cipher=crypto::encrypt(&state.cfg.encryption_key,&payload.ctu_password).map_err(|_|ApiError::internal())?;
    let result=sqlx::query("INSERT INTO users(username,password_hash,ctu_username,ctu_password_ciphertext) VALUES(?,?,?,?)")
        .bind(&payload.username).bind(hash).bind(&payload.ctu_username).bind(&cipher).execute(&state.db).await;
    let id=match result {Ok(v)=>v.last_insert_rowid(),Err(sqlx::Error::Database(e)) if e.is_unique_violation()=>return Err(ApiError::conflict("Username already exists")),Err(e)=>return Err(e.into())};
    let headers=create_session(&state,id).await?;
    Ok((StatusCode::CREATED,headers,Json(PublicUser{id,username:payload.username,ctu_username:payload.ctu_username,telegram_linked:false,telegram_username:None})))
}

#[derive(Deserialize)]
pub struct LoginRequest {username:String,password:String}
pub async fn login(State(state):State<AppState>, headers:HeaderMap, Json(p):Json<LoginRequest>) -> Result<(HeaderMap,Json<PublicUser>),ApiError> {
    check_origin(&headers,&state)?;
    if !valid_username(&p.username) || p.password.len()>256{return Err(ApiError::unauthorized());}
    let now=Utc::now().timestamp();
    sqlx::query("DELETE FROM login_attempts WHERE attempted_at<?").bind(now-900).execute(&state.db).await?;
    let attempts:i64=sqlx::query_scalar("SELECT count(*) FROM login_attempts WHERE username=? AND attempted_at>=?")
        .bind(&p.username).bind(now-900).fetch_one(&state.db).await?;
    if attempts>=5{return Err(ApiError(StatusCode::TOO_MANY_REQUESTS,"Too many attempts. Try again in 15 minutes".into()));}
    let row=sqlx::query("SELECT id,username,password_hash,ctu_username,telegram_chat_id,telegram_username FROM users WHERE username=?")
        .bind(&p.username).fetch_optional(&state.db).await?;
    let valid=if let Some(ref u)=row {
        let encoded:String=u.get("password_hash");
        PasswordHash::new(&encoded).ok().map(|parsed|Argon2::default().verify_password(p.password.as_bytes(),&parsed).is_ok()).unwrap_or(false)
    } else { false };
    if !valid {
        sqlx::query("INSERT INTO login_attempts(username,attempted_at) VALUES(?,?)").bind(&p.username).bind(now).execute(&state.db).await?;
        return Err(ApiError::unauthorized());
    }
    sqlx::query("DELETE FROM login_attempts WHERE username=?").bind(&p.username).execute(&state.db).await?;
    let u=row.expect("validated user");
    let id:i64=u.get("id");
    let headers=create_session(&state,id).await?;
    Ok((headers,Json(PublicUser{id,username:u.get("username"),ctu_username:u.get("ctu_username"),telegram_linked:u.get::<Option<i64>,_>("telegram_chat_id").is_some(),telegram_username:u.get("telegram_username")})))
}

pub async fn me(State(state):State<AppState>,headers:HeaderMap)->Result<Json<PublicUser>,ApiError>{
    Ok(Json(authenticate(&state,&headers).await?.into()))
}
pub async fn logout(State(state):State<AppState>,headers:HeaderMap)->Result<HeaderMap,ApiError>{
    check_origin(&headers,&state)?;
    let cookie_header=headers.get(COOKIE).and_then(|x|x.to_str().ok()).unwrap_or("");
    if let Some(token)=cookie_header.split(';').filter_map(|x|x.trim().split_once('=')).find_map(|(k,v)|if k=="ctu_session"{Some(v)}else{None}) {
        sqlx::query("DELETE FROM sessions WHERE token_hash=?").bind(crypto::digest(token)).execute(&state.db).await?;
    }
    Ok(cookie("",state.cfg.cookie_secure,0))
}
