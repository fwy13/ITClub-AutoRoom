use crate::{schedule::{self, Slot}, state::AppState};
use reqwest::{header::{COOKIE, SET_COOKIE, ACCEPT, CONTENT_TYPE, REFERER, ORIGIN}, Method};
use scraper::{Html, Selector};
use std::{collections::BTreeMap, time::Instant};

#[derive(Debug)]
pub enum Outcome { Confirmed, Failed(String), Uncertain(String) }

#[derive(Default)]
struct CookieJar { cookies: BTreeMap<String,String> }
impl CookieJar {
    fn capture(&mut self, headers: &reqwest::header::HeaderMap) {
        for v in headers.get_all(SET_COOKIE) {
            let Ok(s) = v.to_str() else {continue};
            let first = s.split(';').next().unwrap_or("");
            let Some((name,value)) = first.split_once('=') else {continue};
            let name = name.trim();
            if !name.is_empty() {
                if value.is_empty() {self.cookies.remove(name);} else {self.cookies.insert(name.to_string(),value.to_string());}
            }
        }
    }
    fn header(&self) -> String { self.cookies.iter().map(|(k,v)|format!("{k}={v}")).collect::<Vec<_>>().join("; ") }
}

fn notice(body:&str, expected:&str) -> bool {
    let html=Html::parse_fragment(body);
    let selector=Selector::parse("div.tb_ok").unwrap();
    html.select(&selector).any(|e| e.text().collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")==expected)
}

async fn request(state: &AppState, jar: &mut CookieJar, path: &str, form: Option<&[(&str,&str)]>) -> Result<(u16,String), String> {
    let base=state.cfg.ctu_base.trim_end_matches('/');
    let url=if path.is_empty() {format!("{base}/")} else {format!("{base}/{path}")};
    let mut builder=state.http.request(if form.is_some(){Method::POST}else{Method::GET},url)
        .header(ACCEPT, if form.is_some(){"*/*"}else{"text/html"});
    if let Some(data)=form {
        builder=builder.form(data).header(CONTENT_TYPE,"application/x-www-form-urlencoded; charset=UTF-8")
            .header("X-Requested-With","XMLHttpRequest")
            .header(ORIGIN,"https://libapp.ctu.edu.vn")
            .header(REFERER,format!("{base}/"));
    }
    let header=jar.header();
    if !header.is_empty(){builder=builder.header(COOKIE,header);}
    // Only metadata is recorded. Never log the request form, credentials,
    // session cookies, response body, or CTU tooltip student identifiers.
    let start = Instant::now();
    let method = if form.is_some() { "POST" } else { "GET" };
    let endpoint = if path.is_empty() { "/" } else { path };
    let res = match builder.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(stage="ctu_http", endpoint, method,
                elapsed_ms=start.elapsed().as_millis() as u64, error_kind=?error.status(),
                "CTU HTTP request failed before response");
            return Err(format!("CTU network failure: {error}"));
        }
    };
    jar.capture(res.headers());
    let status = res.status().as_u16();
    let body = res.text().await.map_err(|error| {
        tracing::warn!(stage="ctu_http", endpoint, method, http_status=status,
            elapsed_ms=start.elapsed().as_millis() as u64,
            "CTU response could not be decoded");
        format!("CTU response unavailable: {error}")
    })?;
    tracing::debug!(stage="ctu_http", endpoint, method, http_status=status,
        elapsed_ms=start.elapsed().as_millis() as u64,
        "CTU HTTP request completed");
    Ok((status,body))
}

#[tracing::instrument(name="ctu_booking", skip(state, username, password, slot, note, booking_id, job_id),
    fields(job_id=?job_id, booking_id=booking_id, room_id=slot.room_id, booking_date=%slot.date, shift=slot.slot))]
pub async fn book(
    state:&AppState, username:&str, password:&str, slot:&Slot,
    students:i64, note:&str, booking_id:i64, job_id:Option<i64>,
) -> Outcome {
    let mut jar=CookieJar::default();
    tracing::info!(stage="session", "Starting CTU booking flow");
    match request(state,&mut jar,"",None).await {
        Ok((200..=299,_))=> tracing::debug!(stage="session", "CTU session initialized"),
        Ok((status,_))=>{
            tracing::warn!(stage="session", http_status=status, "CTU session initialization rejected");
            return Outcome::Failed("Could not initialize a CTU login session".into());
        },
        Err(_) => {
            tracing::warn!(stage="session", "CTU session initialization network failure");
            return Outcome::Failed("Could not initialize a CTU login session".into());
        },
    }
    let login = request(state,&mut jar,"dangnhap.php",Some(&[("txtusername",username),("txtpassword",password)])).await;
    match login {
        Ok((200..=299,body)) if notice(&body,"Xin chào :)")=> {
            tracing::info!(stage="login", outcome="accepted", "CTU login confirmed");
        },
        Ok((status,_))=>{
            tracing::warn!(stage="login", outcome="rejected", http_status=status,
                "CTU login not confirmed; check credentials without logging them");
            return Outcome::Failed("CTU login was rejected; check your CTU credentials".into());
        },
        Err(_)=>{
            tracing::warn!(stage="login", outcome="network_error", "CTU login request failed");
            return Outcome::Failed("CTU login request failed".into());
        },
    }
    // The public schedule is fetched without a CTU session. Recheck using the
    // SAME authenticated cookie jar that will submit the booking. Do not infer
    // that `catrong_khl` means reserved: it means empty but not clickable in
    // that view. The CTU booking endpoint makes the final eligibility decision.
    let authed_calendar = match request(state, &mut jar, "lich.php", None).await {
        Ok((200..=299, body)) => match schedule::parse(&body) {
            Ok(calendar) => calendar,
            Err(e) => {
                tracing::warn!(stage="authenticated_timetable", parse_error=%e,
                    "CTU authenticated timetable format is invalid");
                return Outcome::Failed("Cannot validate the logged-in CTU timetable".into());
            }
        },
        Ok((http_status,_)) => {
            tracing::warn!(stage="authenticated_timetable", http_status,
                "CTU authenticated timetable returned an error");
            return Outcome::Failed("Cannot load the logged-in CTU timetable".into());
        },
        Err(_) => {
            tracing::warn!(stage="authenticated_timetable", "CTU authenticated timetable request failed");
            return Outcome::Failed("Cannot load the logged-in CTU timetable".into());
        },
    };
    let source_slot = authed_calendar.slots.iter().find(|s|
        s.room_id == slot.room_id && s.date == slot.date && s.slot == slot.slot);
    tracing::info!(stage="authenticated_timetable", slot_status=?source_slot.map(|s| s.status.as_str()),
        "CTU authenticated timetable occupancy check");
    match source_slot.map(|s| s.status.as_str()) {
        Some("available") | Some("vacant") => {},
        Some("reserved") => return Outcome::Failed("CTU reports this shift as already reserved".into()),
        Some("closed") | Some("unavailable") => return Outcome::Failed("CTU reports this shift as closed or expired".into()),
        _ => return Outcome::Failed("The shift is missing from the logged-in CTU timetable".into()),
    }
    let date=match chrono::NaiveDate::parse_from_str(&slot.date,"%Y-%m-%d") {
        Ok(d)=>d.format("%d-%m-%Y").to_string(),
        Err(_)=>return Outcome::Failed("Invalid date".into()),
    };
    let room=slot.room_id.to_string();
    let shift=slot.slot.to_string();
    let count=students.to_string();
    // This POST follows the fields present in the user's original CTU worker.
    let payload=[("id_p",room.as_str()),("id_c",shift.as_str()),("ngay_hoc",date.as_str()),("sv_n",count.as_str()),("gc",note)];
    tracing::info!(stage="submission", "Sending CTU booking POST once");
    match request(state,&mut jar,"dangky.php",Some(&payload)).await {
        Ok((200..=299,body)) if notice(&body,"Đăng ký thành công :)")=>{
            tracing::info!(stage="submission", outcome="confirmed", "CTU confirmed reservation");
            Outcome::Confirmed
        },
        Ok((http_status,_)) if (200..=299).contains(&http_status)=>{
            tracing::warn!(stage="submission", outcome="uncertain", http_status,
                "CTU response did not contain exact success notice; do not retry automatically");
            Outcome::Uncertain("CTU did not return its exact confirmation; verify on the official website before retrying".into())
        },
        Ok((http_status,_))=>{
            tracing::warn!(stage="submission", outcome="uncertain", http_status,
                "CTU returned non-success HTTP status after POST; do not retry automatically");
            Outcome::Uncertain("CTU returned an error after submission; verify before retrying".into())
        },
        Err(_)=>{
            tracing::warn!(stage="submission", outcome="uncertain",
                "CTU POST response missing; verify official CTU before any retry");
            Outcome::Uncertain("The CTU response was lost after submission; verify before retrying".into())
        },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn success_must_match_explicit_ctu_notice() {
        assert!(super::notice("<div class='tb_ok'>Đăng ký thành công :)</div>","Đăng ký thành công :)"));
        assert!(!super::notice("<div class='tb_err'>Đăng ký thành công :)</div>","Đăng ký thành công :)"));
    }
}
