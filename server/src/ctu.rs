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
    // SAME authenticated cookie jar that will submit the booking. `catrong_khl`
    // means empty but NOT clickable. Immediate bookings preserve the original
    // attempt-and-confirm behavior; scheduled jobs require an explicitly open
    // slot because a premature POST cannot be retried safely.
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
        Some("available") => {},
        Some("vacant") if job_id.is_none() => {},
        Some("vacant") => return Outcome::Failed(
            "The authenticated CTU shift is no longer bookable; no booking POST sent".into()),
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

/// A read-only booking eligibility check for **advance jobs**. The only POST
/// here is authentication (`dangnhap.php`); this never calls `dangky.php`.
/// An anonymous `vacant` cell is not evidence that booking has opened.
/// Treat ambiguous CTU replies as "waiting", not as permission to submit.
#[derive(Debug)]
pub enum PreflightOutcome {
    Ready,
    Waiting(String),
    Blocked(String),
}

fn interpret_authenticated_status(status: Option<&str>) -> PreflightOutcome {
    match status {
        Some("available") => PreflightOutcome::Ready,
        Some("reserved") | Some("closed") => PreflightOutcome::Blocked(
            "CTU reports this shift as reserved or closed".into()),
        // `catrong_khl` is vacant but not bookable in this authenticated view.
        // Do not consume the only POST attempt before CTU visibly opens it.
        _ => PreflightOutcome::Waiting("CTU has not exposed a bookable shift for this account".into()),
    }
}

pub async fn preflight_authenticated(
    state: &AppState, username: &str, password: &str,
    room_id: i64, shift: i64, date: &str,
) -> PreflightOutcome {
    let mut jar = CookieJar::default();
    match request(state, &mut jar, "", None).await {
        Ok((200..=299, _)) => {},
        _ => return PreflightOutcome::Waiting("CTU login session is temporarily unavailable".into()),
    }
    match request(state, &mut jar, "dangnhap.php", Some(&[
        ("txtusername", username), ("txtpassword", password),
    ])).await {
        Ok((200..=299, body)) if notice(&body, "Xin chào :)") => {},
        Ok((200..=299, _)) => return PreflightOutcome::Blocked("CTU login not confirmed; check CTU credentials".into()),
        _ => return PreflightOutcome::Waiting("CTU login service unavailable".into()),
    }
    let calendar = match request(state, &mut jar, "lich.php", None).await {
        Ok((200..=299, body)) => match schedule::parse(&body) {
            Ok(calendar) => calendar,
            Err(_) => return PreflightOutcome::Waiting("Cannot parse the authenticated CTU timetable".into()),
        },
        _ => return PreflightOutcome::Waiting("Cannot read the authenticated CTU timetable".into()),
    };
    let status = calendar.slots.iter().find(|s|
        s.room_id == room_id && s.slot == shift && s.date == date
    ).map(|s| s.status.as_str());
    tracing::info!(stage="advance_authenticated_preflight", room_id, shift, booking_date=date,
        slot_status=?status, "Read-only check before an advance booking POST");
    interpret_authenticated_status(status)
}

/// Cancellation is a distinct one-shot CTU operation. The form uses multipart
/// boundary encoding ("form-data"), not the urlencoded booking/login format.
#[derive(Debug)]
pub enum CancellationOutcome { Confirmed, Rejected(String), Uncertain(String) }

#[tracing::instrument(name="ctu_cancellation", skip(state, username, password),
    fields(room_id, shift, booking_date))]
pub async fn cancel(
    state: &AppState, username: &str, password: &str,
    room_id: i64, shift: i64, date: &str,
) -> CancellationOutcome {
    let date = match chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Ok(d) => d.format("%d-%m-%Y").to_string(),
        Err(_) => return CancellationOutcome::Rejected("Invalid booking date".into()),
    };
    let mut jar = CookieJar::default();
    // A failure before the cancellation POST is safe to report as rejected:
    // no cancellation could have been submitted to CTU.
    match request(state, &mut jar, "", None).await {
        Ok((200..=299, _)) => {},
        _ => return CancellationOutcome::Rejected("Cannot initialize CTU cancellation session".into()),
    }
    match request(state, &mut jar, "dangnhap.php", Some(&[
        ("txtusername", username), ("txtpassword", password),
    ])).await {
        Ok((200..=299, body)) if notice(&body, "Xin chào :)") => {},
        _ => return CancellationOutcome::Rejected("CTU login was not confirmed; cancellation was not submitted".into()),
    }

    let endpoint = format!("{}/huydk.php", state.cfg.ctu_base.trim_end_matches('/'));
    let form = reqwest::multipart::Form::new()
        .text("id_p", room_id.to_string())
        .text("id_c", shift.to_string())
        .text("ngay_hoc", date);
    let mut builder = state.http.post(endpoint)
        .header(ACCEPT, "*/*")
        .header("X-Requested-With", "XMLHttpRequest")
        .header(ORIGIN, "https://libapp.ctu.edu.vn")
        .header(REFERER, format!("{}/", state.cfg.ctu_base.trim_end_matches('/')))
        .multipart(form);
    let cookie = jar.header();
    if !cookie.is_empty() { builder = builder.header(COOKIE, cookie); }
    // From this point even a timeout can mean CTU processed the cancellation.
    // Exactly one POST; do not retry automatically under any circumstances.
    tracing::info!(stage="cancellation_submission", "Sending one authenticated CTU multipart POST");
    let response = match builder.send().await {
        Ok(response) => response,
        Err(_) => return CancellationOutcome::Uncertain(
            "CTU cancellation response was lost; verify on the official site".into()),
    };
    let code = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(_) => return CancellationOutcome::Uncertain(
            "Unable to read CTU cancellation response; verify on the official site".into()),
    };
    if code.is_success() && notice(&body, "Hủy thành công :)") {
        tracing::info!(stage="cancellation_submission", outcome="confirmed", "CTU confirmed cancellation");
        return CancellationOutcome::Confirmed;
    }
    // Only an explicit negative notice counts as a definite rejection.
    // Unknown HTML and non-2xx responses remain ambiguous after POST.
    if code.is_success() && has_error_notice(&body) {
        return CancellationOutcome::Rejected("CTU explicitly rejected cancellation; the booking remains confirmed".into());
    }
    tracing::warn!(stage="cancellation_submission", outcome="uncertain", http_status=%code,
        "CTU did not provide the exact cancellation success notice");
    CancellationOutcome::Uncertain(
        "CTU did not confirm cancellation; verify on the official site before another attempt".into())
}

fn has_error_notice(body: &str) -> bool {
    let html = Html::parse_fragment(body);
    let selector = Selector::parse("div.tb_err, div.tb_error").unwrap();
    html.select(&selector).any(|node| node.text().any(|text| !text.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn cancellation_is_authenticated_multipart_with_exact_fields() {
        use crate::{config::Config, state::AppState};
        use axum::{body::Bytes, http::HeaderMap, routing::{get, post}, Router};
        use sqlx::sqlite::SqlitePoolOptions;
        use std::{sync::{Arc, atomic::AtomicI64}, time::Duration};
        use tokio::sync::Mutex;
        let seen = Arc::new(Mutex::new(None::<(String, String, String)>));
        let recording = Arc::clone(&seen);
        let app = Router::new()
            .route("/dkptl/", get(|| async {
                ([(axum::http::header::SET_COOKIE, "PHPSESSID=mock-session; Path=/")], "ready")
            }))
            .route("/dkptl/dangnhap.php", post(|| async { "<div class='tb_ok'>Xin chào :)</div>" }))
            .route("/dkptl/huydk.php", post(move |headers: HeaderMap, body: Bytes| {
                let recording = Arc::clone(&recording);
                async move {
                    let typ = headers.get(axum::http::header::CONTENT_TYPE).unwrap().to_str().unwrap().to_string();
                    let cookie = headers.get(axum::http::header::COOKIE).unwrap().to_str().unwrap().to_string();
                    let raw = String::from_utf8(body.to_vec()).unwrap();
                    *recording.lock().await = Some((typ,cookie,raw));
                    "<div class='tb_ok'>Hủy thành công :)</div>"
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let db = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        let cfg = Config {
            bind:"127.0.0.1:0".into(), database_url:"sqlite::memory:".into(),
            encryption_key:[0;32], ctu_base:format!("http://{addr}/dkptl"),
            allowed_origins:vec![], cookie_secure:false, telegram_token:None,
            telegram_username:None, refresh_secs:120, seed_fixture:false,
        };
        let state=AppState {
            db, http:reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            cfg:Arc::new(cfg), booking_gate:Arc::new(Mutex::new(())), sync_gate:Arc::new(Mutex::new(())),
            scheduler_heartbeat:Arc::new(AtomicI64::new(0)), scheduler_last_db_ok:Arc::new(AtomicI64::new(0)),
            scheduler_notify:Arc::new(tokio::sync::Notify::new()),
        };
        let result = super::cancel(&state,"student","password",1,1,"2026-11-23").await;
        assert!(matches!(result, super::CancellationOutcome::Confirmed));
        let (content_type,cookie,body) = seen.lock().await.clone().unwrap();
        assert!(content_type.starts_with("multipart/form-data; boundary="));
        assert!(cookie.contains("PHPSESSID=mock-session"));
        for (name,value) in [("id_p","1"),("id_c","1"),("ngay_hoc","23-11-2026")] {
            assert!(body.contains(&format!("name=\"{name}\"")));
            assert!(body.contains(&format!("\r\n\r\n{value}\r\n")));
        }
        server.abort();
    }

    #[tokio::test]
    async fn advance_preflight_does_not_submit_booking_for_authenticated_vacancy() {
        use crate::{config::Config, state::AppState};
        use axum::{routing::{get, post}, Router};
        use sqlx::sqlite::SqlitePoolOptions;
        use std::{sync::{Arc, atomic::{AtomicI64, AtomicUsize, Ordering}}, time::Duration};
        use tokio::sync::Mutex;
        let submits = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&submits);
        let page = include_str!("../../fixtures/lich-2026-11-05.html").to_string();
        let router = Router::new()
            .route("/dkptl/", get(|| async {
                ([(axum::http::header::SET_COOKIE, "PHPSESSID=test; Path=/")], "ready")
            }))
            .route("/dkptl/dangnhap.php", post(|| async {
                "<div class='tb_ok'>Xin chào :)</div>"
            }))
            .route("/dkptl/lich.php", get(move || {
                let html = page.clone();
                async move { html }
            }))
            .route("/dkptl/dangky.php", post(move || {
                let observed = Arc::clone(&observed);
                async move {
                    observed.fetch_add(1, Ordering::Relaxed);
                    "unexpected booking submission"
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        let db = SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
        let cfg = Config {
            bind:"127.0.0.1:0".into(), database_url:"sqlite::memory:".into(),
            encryption_key:[0;32], ctu_base:format!("http://{address}/dkptl"),
            allowed_origins:vec![], cookie_secure:false, telegram_token:None,
            telegram_username:None, refresh_secs:120, seed_fixture:false,
        };
        let state = AppState {
            db, http:reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            cfg:Arc::new(cfg), booking_gate:Arc::new(Mutex::new(())),
            sync_gate:Arc::new(Mutex::new(())),
            scheduler_heartbeat:Arc::new(AtomicI64::new(0)),
            scheduler_last_db_ok:Arc::new(AtomicI64::new(0)),
            scheduler_notify:Arc::new(tokio::sync::Notify::new()),
        };
        let outcome = super::preflight_authenticated(
            &state, "student", "password", 1, 1, "2026-11-13").await;
        assert!(matches!(outcome, super::PreflightOutcome::Waiting(_)));
        assert_eq!(submits.load(Ordering::Relaxed), 0);
        server.abort();
    }

    #[test]
    fn authenticated_status_is_exhaustive_and_fail_closed() {
        assert!(matches!(super::interpret_authenticated_status(Some("available")),
            super::PreflightOutcome::Ready));
        assert!(matches!(super::interpret_authenticated_status(Some("vacant")),
            super::PreflightOutcome::Waiting(_)));
        assert!(matches!(super::interpret_authenticated_status(Some("unexpected_status")),
            super::PreflightOutcome::Waiting(_)));
        assert!(matches!(super::interpret_authenticated_status(None),
            super::PreflightOutcome::Waiting(_)));
        assert!(matches!(super::interpret_authenticated_status(Some("reserved")),
            super::PreflightOutcome::Blocked(_)));
        assert!(matches!(super::interpret_authenticated_status(Some("closed")),
            super::PreflightOutcome::Blocked(_)));
    }

    #[test]
    fn anonymous_empty_slot_does_not_imply_authorized_open_slot() {
        let calendar = crate::schedule::parse(include_str!("../../fixtures/lich-2026-11-05.html")).unwrap();
        let cell = calendar.slots.iter().find(|s|
            s.room_id == 1 && s.date == "2026-11-13" && s.slot == 1).unwrap();
        assert_eq!(cell.status, "vacant");
        // The advance authenticated preflight permits only explicit "available".
        assert_ne!(cell.status, "available");
    }

    #[test]
    fn success_must_match_explicit_ctu_notice() {
        assert!(super::notice("<div class='tb_ok'>Đăng ký thành công :)</div>","Đăng ký thành công :)"));
        assert!(!super::notice("<div class='tb_err'>Đăng ký thành công :)</div>","Đăng ký thành công :)"));
        assert!(super::notice("<div class='tb_ok'>Hủy thành công :)</div>", "Hủy thành công :)"));
        assert!(!super::notice("<div class='tb_ok'>Không thành công</div>", "Hủy thành công :)"));
        assert!(super::has_error_notice("<div class='tb_err'>Không thể hủy</div>"));
    }
}
