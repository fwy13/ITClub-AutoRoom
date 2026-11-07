use crate::{auth, crypto, error::ApiError, state::AppState};
use axum::{extract::State, http::HeaderMap, Json};
use chrono::Utc;
use serde::Serialize;
use serde_json::{Value,json};
use sqlx::Row;
use tokio::time::{sleep,Duration};

#[derive(Serialize)]
pub struct LinkResponse { url:String, expires_in_seconds:i64 }

pub async fn create_link(State(state):State<AppState>,headers:HeaderMap)->Result<Json<LinkResponse>,ApiError>{
    auth::check_origin(&headers,&state)?;
    let user=auth::authenticate(&state,&headers).await?;
    if state.cfg.telegram_token.is_none(){return Err(ApiError::unavailable("Telegram bot has not been configured"));}
    let name=state.cfg.telegram_username.as_deref().ok_or_else(||ApiError::unavailable("TELEGRAM_BOT_USERNAME is not set"))?;
    let code=crypto::random_token();
    sqlx::query("UPDATE users SET telegram_link_hash=?,telegram_link_expires=? WHERE id=?")
        .bind(crypto::digest(&code)).bind(Utc::now().timestamp()+600).bind(user.id).execute(&state.db).await?;
    Ok(Json(LinkResponse {url:format!("https://t.me/{name}?start={code}"),expires_in_seconds:600}))
}

pub async fn unlink(State(state):State<AppState>,headers:HeaderMap)->Result<Json<Value>,ApiError>{
    auth::check_origin(&headers,&state)?;
    let user=auth::authenticate(&state,&headers).await?;
    let mut tx=state.db.begin().await?;
    sqlx::query("UPDATE users SET telegram_chat_id=NULL,telegram_username=NULL,telegram_link_hash=NULL,telegram_link_expires=NULL WHERE id=?")
        .bind(user.id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM notifications WHERE sent_at IS NULL AND booking_id IN (SELECT id FROM bookings WHERE user_id=?)")
        .bind(user.id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"ok":true})))
}

async fn send_message(state:&AppState,chat_id:i64,body:&str)->Result<(),String>{
    let token=state.cfg.telegram_token.as_ref().ok_or("Bot not configured")?;
    let response=state.http.post(format!("https://api.telegram.org/bot{token}/sendMessage"))
        .json(&json!({"chat_id":chat_id,"text":body})).send().await.map_err(|e|e.to_string())?;
    if !response.status().is_success(){return Err(format!("Telegram HTTP {}",response.status()));}
    let data:Value=response.json().await.map_err(|e|e.to_string())?;
    if data["ok"] != true { return Err("Telegram API rejected the message".into()); }
    Ok(())
}

pub async fn notify_job_failure(state:&AppState,user_id:i64,message:String){
    if state.cfg.telegram_token.is_none(){return;}
    let chat:Option<i64>=match sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id=?").bind(user_id).fetch_optional(&state.db).await {
        Ok(c)=>c.flatten(),Err(_)=>None
    };
    if let Some(chat_id)=chat {
        if let Err(e)=send_message(state,chat_id,&message).await {tracing::warn!(error=%e,"Could not notify about scheduled job failure");}
    }
}

pub async fn enqueue(state:&AppState,booking_id:i64,user_id:i64,body:String)->Result<(),ApiError>{
    if state.cfg.telegram_token.is_none(){return Ok(());}
    let chat:Option<i64>=sqlx::query_scalar("SELECT telegram_chat_id FROM users WHERE id=?")
        .bind(user_id).fetch_optional(&state.db).await?.flatten();
    if let Some(id)=chat {
        sqlx::query("INSERT OR IGNORE INTO notifications (booking_id,chat_id,body) VALUES(?,?,?)")
            .bind(booking_id).bind(id).bind(body).execute(&state.db).await?;
    }
    Ok(())
}

pub async fn deliver_pending(state:&AppState){
    if state.cfg.telegram_token.is_none(){return;}
    let rows=match sqlx::query("SELECT id,chat_id,body,attempts FROM notifications WHERE sent_at IS NULL AND attempts < 6 AND next_attempt_at<=? ORDER BY id LIMIT 10")
        .bind(Utc::now().timestamp()).fetch_all(&state.db).await {Ok(r)=>r,Err(e)=>{tracing::warn!(error=%e,"notification query failed");return;}};
    for r in rows {
        let id:i64=r.get("id");
        let chat:i64=r.get("chat_id");
        let body:String=r.get("body");
        let attempts:i64=r.get("attempts");
        match send_message(state,chat,&body).await {
            Ok(_)=>{let _=sqlx::query("UPDATE notifications SET sent_at=? WHERE id=?")
                .bind(Utc::now().timestamp()).bind(id).execute(&state.db).await;}
            Err(e)=>{
                tracing::warn!(notification_id=id,error=%e,"Telegram delivery failed");
                let delay=30i64*2i64.pow(attempts.min(5) as u32);
                let _=sqlx::query("UPDATE notifications SET attempts=attempts+1,next_attempt_at=? WHERE id=?")
                    .bind(Utc::now().timestamp()+delay).bind(id).execute(&state.db).await;
            }
        }
    }
}

async fn handle_message(state:&AppState,msg:&Value) {
    let Some(chat_id)=msg.pointer("/chat/id").and_then(Value::as_i64) else {return};
    // Never link a group chat; notifications contain private reservation details.
    if msg.pointer("/chat/type").and_then(Value::as_str)!=Some("private"){return;}
    let Some(text)=msg.get("text").and_then(Value::as_str) else {return};
    let mut parts=text.split_whitespace();
    let command=parts.next().unwrap_or("");
    if !command.starts_with("/start"){return;}
    let Some(code)=parts.next() else {
        let _=send_message(state,chat_id,"Open ITClub-AutoRoom, sign in, and choose Link Telegram to receive your personal link.").await;
        return;
    };
    if code.len()!=64 || !code.bytes().all(|c|c.is_ascii_hexdigit()) {return;}
    let username=msg.pointer("/from/username").and_then(Value::as_str).map(ToString::to_string);
    let res=sqlx::query("UPDATE users SET telegram_chat_id=?,telegram_username=?,telegram_link_hash=NULL,telegram_link_expires=NULL WHERE telegram_link_hash=? AND telegram_link_expires>?")
        .bind(chat_id).bind(username).bind(crypto::digest(code)).bind(Utc::now().timestamp()).execute(&state.db).await;
    let linked=matches!(res,Ok(ref v) if v.rows_affected()==1);
    let message=if linked {"✅ Telegram connected. You will receive reservation outcomes here."} else {"Link expired or invalid. Generate a new one in the app."};
    let _=send_message(state,chat_id,message).await;
}

pub async fn poll_bot(state:AppState){
    let Some(token)=state.cfg.telegram_token.clone() else {return};
    let mut offset:i64=0;
    loop {
        let res=state.http.get(format!("https://api.telegram.org/bot{token}/getUpdates"))
            .query(&[("offset",offset.to_string()),("timeout","15".into()),("allowed_updates","[\"message\"]".into())])
            .timeout(Duration::from_secs(23)).send().await;
        match res {
            Ok(res) if res.status().is_success()=> {
                if let Ok(data)=res.json::<Value>().await {
                    if let Some(items)=data["result"].as_array() {
                        for item in items {
                            if let Some(id)=item["update_id"].as_i64() {offset=offset.max(id+1);}
                            if let Some(msg)=item.get("message"){handle_message(&state,msg).await;}
                        }
                    }
                }
            }
            Ok(res)=>tracing::warn!(status=%res.status(),"Telegram getUpdates failed; remove any existing webhook"),
            Err(e)=>tracing::warn!(error=%e,"Telegram polling error"),
        }
        sleep(Duration::from_secs(2)).await;
    }
}

pub async fn notification_loop(state:AppState){
    loop {deliver_pending(&state).await;sleep(Duration::from_secs(15)).await;}
}
