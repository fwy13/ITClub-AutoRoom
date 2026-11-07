use crate::{schedule::Slot, state::AppState};
use reqwest::{header::{COOKIE, SET_COOKIE, ACCEPT, CONTENT_TYPE, REFERER, ORIGIN}, Method};
use scraper::{Html, Selector};
use std::collections::BTreeMap;

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
    let res=builder.send().await.map_err(|e|format!("CTU network failure: {e}"))?;
    jar.capture(res.headers());
    let status=res.status().as_u16();
    let body=res.text().await.map_err(|e|format!("CTU response unavailable: {e}"))?;
    Ok((status,body))
}

pub async fn book(state:&AppState, username:&str, password:&str, slot:&Slot, students:i64, note:&str) -> Outcome {
    let mut jar=CookieJar::default();
    match request(state,&mut jar,"",None).await {
        Ok((200..=299,_))=>{},
        _=>return Outcome::Failed("Could not initialize a CTU login session".into()),
    }
    let login = request(state,&mut jar,"dangnhap.php",Some(&[("txtusername",username),("txtpassword",password)])).await;
    match login {
        Ok((200..=299,body)) if notice(&body,"Xin chào :)")=>{},
        Ok(_)=>return Outcome::Failed("CTU login was rejected; check your CTU credentials".into()),
        Err(_)=>return Outcome::Failed("CTU login request failed".into()),
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
    match request(state,&mut jar,"dangky.php",Some(&payload)).await {
        Ok((200..=299,body)) if notice(&body,"Đăng ký thành công :)")=>Outcome::Confirmed,
        Ok((200..=299,_))=>Outcome::Uncertain("CTU did not return its exact confirmation; verify on the official website before retrying".into()),
        Ok(_)=>Outcome::Uncertain("CTU returned an error after submission; verify before retrying".into()),
        Err(_)=>Outcome::Uncertain("The CTU response was lost after submission; verify before retrying".into()),
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
