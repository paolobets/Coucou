// Whoop — the Rust side of the `integration_whoop` pill.
//
// Whoop has no API key: an app of the user's own (developer.whoop.com) gives
// a client id and secret, and the user signs in once in the browser. Coucou
// listens on a loopback port for the redirect, trades the code for tokens and
// keeps them in the Credential Manager / Secret Service as Rust-only entries
// (secrets::INTERNAL_KEYS: no webview can read them). Whoop rotates the
// refresh token on every refresh, so each refresh is written back at once.
//
// The poller then reads the latest recovery, cycle (today's strain), sleep and
// workout every 15 minutes: a recovery is scored once a day, strain moves
// slowly. The card badges and Mochi reacts when a new recovery is scored.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tauri::AppHandle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::integrations::{self, is_new, IntegrationEvent, IntegrationUpdate};
use crate::secrets;

pub const PILL: &str = "integration_whoop";
/// The loopback port the browser comes back to; the app registered at Whoop
/// must list exactly `REDIRECT_URI`.
pub const PORT: u16 = 47431;
pub const REDIRECT_URI: &str = "http://localhost:47431/whoop";
const AUTH_URL: &str = "https://api.prod.whoop.com/oauth/oauth2/auth";
const TOKEN_URL: &str = "https://api.prod.whoop.com/oauth/oauth2/token";
const API: &str = "https://api.prod.whoop.com/developer";
/// `offline` is what gives a refresh token.
const SCOPES: &str = "read:recovery read:sleep read:workout read:cycles read:profile offline";
/// How long the sign-in may take in the browser.
const SIGN_IN: Duration = Duration::from_secs(300);

pub const CLIENT_ID: &str = "whoop-client-id";
pub const CLIENT_SECRET: &str = "whoop-client-secret";
const ACCESS: &str = "whoop-access-token";
const REFRESH: &str = "whoop-refresh-token";
const EXPIRES: &str = "whoop-expires-at";

/// One sign-in at a time.
static CONNECTING: AtomicBool = AtomicBool::new(false);

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

struct Tokens {
    access: String,
    refresh: String,
    expires_at: u64,
}

fn tokens() -> Option<Tokens> {
    Some(Tokens {
        access: secrets::get_internal(ACCESS)?,
        refresh: secrets::get_internal(REFRESH)?,
        expires_at: secrets::get_internal(EXPIRES).and_then(|s| s.parse().ok()).unwrap_or(0),
    })
}

/// Stores a token response; Whoop's refresh tokens rotate, so the new one
/// replaces the old at once.
fn store(body: &Value) -> Result<(), String> {
    let access = body.get("access_token").and_then(Value::as_str).ok_or("no access token")?;
    let refresh = body.get("refresh_token").and_then(Value::as_str).ok_or("no refresh token")?;
    let ttl = body.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
    secrets::set_internal(ACCESS, access)?;
    secrets::set_internal(REFRESH, refresh)?;
    secrets::set_internal(EXPIRES, &(now() + ttl).to_string())
}

/// Forgets the sign-in; the client id and secret stay.
pub fn disconnect() {
    for key in [ACCESS, REFRESH, EXPIRES] {
        let _ = secrets::set_internal(key, "");
    }
}

pub fn connected() -> bool {
    tokens().is_some()
}

// ── OAuth ─────────────────────────────────────────────────────────────────────

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where the browser goes to sign in.
pub fn authorize_url(client_id: &str, state: &str) -> String {
    format!(
        "{AUTH_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
        encode(client_id),
        encode(REDIRECT_URI),
        encode(SCOPES),
        encode(state)
    )
}

/// The `code` and `state` of the request line the browser sends back
/// (`GET /whoop?code=…&state=… HTTP/1.1`), or Whoop's error.
pub fn parse_callback(request_line: &str) -> Result<(String, String), String> {
    let path = request_line.split_whitespace().nth(1).ok_or("not an HTTP request")?;
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(decode(v)),
            "state" => state = Some(decode(v)),
            "error" => error = Some(decode(v)),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok((code.ok_or("no code in the redirect")?, state.unwrap_or_default()))
}

fn random_state() -> Result<String, String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).map_err(|_| "no secure random source".to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

async fn token_request(form: &[(&str, &str)]) -> Result<Value, String> {
    let response = integrations::client()
        .post(TOKEN_URL)
        .form(form)
        .send()
        .await
        .map_err(|e| crate::i18n::tf("No connection: {error}", &[("error", &e.to_string())]))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(json!({}));
    if !status.is_success() {
        let detail = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Err(crate::i18n::tf("Whoop refused the connection: {error}", &[("error", &format!("{} {detail}", status.as_u16()))]));
    }
    Ok(body)
}

/// The sign-in: a loopback listener, the browser, the redirect, the tokens.
/// Returns once Coucou holds the tokens (or says why not).
pub async fn connect(app: AppHandle) -> Result<(), String> {
    let (Some(client_id), Some(client_secret)) = (secrets::get(CLIENT_ID), secrets::get(CLIENT_SECRET)) else {
        return Err(crate::i18n::t("Key not configured"));
    };
    if CONNECTING.swap(true, Ordering::SeqCst) {
        return Err(crate::i18n::t("Waiting for Whoop in the browser…"));
    }
    let result = sign_in(&client_id, &client_secret).await;
    CONNECTING.store(false, Ordering::SeqCst);
    result?;
    poll(app).await;
    Ok(())
}

async fn sign_in(client_id: &str, client_secret: &str) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", PORT))
        .await
        .map_err(|_| crate::i18n::tf("Port {port} is taken by another program.", &[("port", &PORT.to_string())]))?;
    let state = random_state()?;
    crate::platform::open_url(&authorize_url(client_id, &state));

    // The browser comes back once. Anything else that knocks on the port
    // (a favicon request, a stray probe) is answered and ignored.
    let deadline = tokio::time::Instant::now() + SIGN_IN;
    let code = loop {
        let accept = tokio::time::timeout_at(deadline, listener.accept()).await;
        let Ok(Ok((mut socket, _))) = accept else {
            return Err(crate::i18n::t("The browser did not come back in time."));
        };
        let mut buf = vec![0u8; 8192];
        let n = match tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let line = request.lines().next().unwrap_or("").to_string();
        if !line.contains("/whoop") {
            let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").await;
            continue;
        }
        let outcome = parse_callback(&line).and_then(|(code, got)| {
            if got == state { Ok(code) } else { Err("state mismatch".into()) }
        });
        let (status, text) = match &outcome {
            Ok(_) => ("200 OK", crate::i18n::t("Whoop connected. You can close this tab and go back to Coucou.")),
            Err(e) => ("400 Bad Request", crate::i18n::tf("Whoop refused the connection: {error}", &[("error", e)])),
        };
        let page = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>Coucou</title></head><body style=\"font-family:system-ui;background:#111;color:#eee;display:flex;align-items:center;justify-content:center;height:100vh;margin:0\"><p>{}</p></body></html>",
            text.replace('<', "&lt;")
        );
        let _ = socket
            .write_all(
                format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{page}", page.len())
                    .as_bytes(),
            )
            .await;
        let _ = socket.shutdown().await;
        break outcome?;
    };

    let body = token_request(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("redirect_uri", REDIRECT_URI),
    ])
    .await?;
    store(&body)
}

/// A valid access token, refreshed when it expires within five minutes.
async fn access_token() -> Result<String, String> {
    let t = tokens().ok_or_else(|| crate::i18n::t("Connect Whoop first."))?;
    if t.expires_at > now() + 300 {
        return Ok(t.access);
    }
    let (Some(client_id), Some(client_secret)) = (secrets::get(CLIENT_ID), secrets::get(CLIENT_SECRET)) else {
        return Err(crate::i18n::t("Key not configured"));
    };
    let body = token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &t.refresh),
        ("client_id", &client_id),
        ("client_secret", &client_secret),
        ("scope", "offline"),
    ])
    .await
    .map_err(|e| {
        // A refresh Whoop rejects is a sign-in that is over: the next card
        // asks to connect again rather than retrying every quarter of an hour.
        if e.contains("401") || e.contains("400") {
            disconnect();
        }
        e
    })?;
    store(&body)?;
    Ok(body.get("access_token").and_then(Value::as_str).unwrap_or_default().to_string())
}

// ── The poll ──────────────────────────────────────────────────────────────────

/// The newest records of a collection (`/v2/recovery`, `/v2/cycle`…).
async fn latest(token: &str, path: &str, limit: u32) -> Result<Vec<Value>, String> {
    let response = integrations::client()
        .get(format!("{API}{path}?limit={limit}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| crate::i18n::tf("No connection: {error}", &[("error", &e.to_string())]))?;
    if !response.status().is_success() {
        return Err(integrations::status_error(response.status().as_u16(), &crate::i18n::t("Token lacks the needed scope")));
    }
    let body: Value = response.json().await.unwrap_or(json!({}));
    Ok(body.get("records").and_then(Value::as_array).cloned().unwrap_or_default())
}

fn scored(record: &Value) -> Option<&Value> {
    (record.get("score_state").and_then(Value::as_str) == Some("SCORED")).then(|| record.get("score")).flatten()
}

/// The card's data from the four collections. Pure, so it can be tested.
pub fn summarize(recoveries: &[Value], cycles: &[Value], sleeps: &[Value], workouts: &[Value]) -> Value {
    let recovery = recoveries.iter().find_map(|r| {
        let s = scored(r)?;
        Some(json!({
            "id": r.get("cycle_id").map(|v| v.to_string()).unwrap_or_default(),
            "score": s.get("recovery_score").and_then(Value::as_f64),
            "hrv": s.get("hrv_rmssd_milli").and_then(Value::as_f64),
            "rhr": s.get("resting_heart_rate").and_then(Value::as_f64),
            "calibrating": s.get("user_calibrating").and_then(Value::as_bool).unwrap_or(false),
            "at": r.get("created_at").and_then(Value::as_str),
        }))
    });
    let strain = cycles.iter().find_map(|c| scored(c)?.get("strain").and_then(Value::as_f64));
    let sleep = sleeps.iter().filter(|s| s.get("nap") != Some(&Value::Bool(true))).find_map(|s| {
        let score = scored(s)?;
        let stages = score.get("stage_summary");
        let ms = ["total_light_sleep_time_milli", "total_rem_sleep_time_milli", "total_slow_wave_sleep_time_milli"]
            .iter()
            .filter_map(|k| stages.and_then(|st| st.get(*k)).and_then(Value::as_f64))
            .sum::<f64>();
        Some(json!({
            "performance": score.get("sleep_performance_percentage").and_then(Value::as_f64),
            "hours": if ms > 0.0 { Some((ms / 3_600_000.0 * 10.0).round() / 10.0) } else { None },
            "end": s.get("end").and_then(Value::as_str),
        }))
    });
    let workout = workouts.iter().find_map(|w| {
        let score = scored(w)?;
        Some(json!({
            "sport": w.get("sport_name").and_then(Value::as_str),
            "strain": score.get("strain").and_then(Value::as_f64),
            "end": w.get("end").and_then(Value::as_str),
        }))
    });
    json!({ "recovery": recovery, "strain": strain, "sleep": sleep, "workout": workout })
}

pub async fn poll(app: AppHandle) {
    if secrets::get(CLIENT_ID).is_none() {
        return;
    }
    let emit_error = |error: String| {
        integrations::emit(&app, IntegrationUpdate { id: PILL, data: json!({}), error: Some(error), event: None });
    };
    let token = match access_token().await {
        Ok(t) => t,
        Err(e) => return emit_error(e),
    };
    let (recoveries, cycles, sleeps, workouts) = match tokio::try_join!(
        latest(&token, "/v2/recovery", 1),
        latest(&token, "/v2/cycle", 1),
        latest(&token, "/v2/activity/sleep", 3),
        latest(&token, "/v2/activity/workout", 1),
    ) {
        Ok(all) => all,
        Err(e) => return emit_error(e),
    };
    let data = summarize(&recoveries, &cycles, &sleeps, &workouts);

    // A recovery scored since the last poll: Mochi says the number.
    let mut event = None;
    if let Some(r) = data.get("recovery").filter(|r| !r.is_null()) {
        let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        if !id.is_empty() && is_new("whoop-recovery", &id) {
            if let Some(score) = r.get("score").and_then(Value::as_f64) {
                event = Some(IntegrationEvent {
                    success: true,
                    label: crate::i18n::tf("Recovery {n} %", &[("n", &(score.round() as i64).to_string())]),
                    detail: None,
                });
            }
        }
    }
    integrations::emit(&app, IntegrationUpdate { id: PILL, data, error: None, event });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_carries_everything_encoded() {
        let url = authorize_url("abc 123", "s1");
        assert!(url.starts_with("https://api.prod.whoop.com/oauth/oauth2/auth?response_type=code&client_id=abc%20123&"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A47431%2Fwhoop"));
        assert!(url.contains("scope=read%3Arecovery%20read%3Asleep"));
        assert!(url.ends_with("&state=s1"));
    }

    #[test]
    fn the_redirect_is_parsed_and_errors_are_kept() {
        assert_eq!(parse_callback("GET /whoop?code=ab%2Fcd&state=xyz HTTP/1.1"), Ok(("ab/cd".into(), "xyz".into())));
        assert_eq!(parse_callback("GET /whoop?state=xyz HTTP/1.1"), Err("no code in the redirect".into()));
        assert_eq!(parse_callback("GET /whoop?error=access_denied&state=xyz HTTP/1.1"), Err("access_denied".into()));
        assert!(parse_callback("garbage").is_err());
    }

    #[test]
    fn the_summary_reads_the_latest_scored_records_and_skips_naps() {
        let recoveries = vec![json!({ "cycle_id": 9, "score_state": "PENDING_SCORE" }), json!({
            "cycle_id": 8, "created_at": "2026-10-20T06:00:00Z", "score_state": "SCORED",
            "score": { "recovery_score": 78.0, "hrv_rmssd_milli": 61.2, "resting_heart_rate": 52.0, "user_calibrating": false }
        })];
        let cycles = vec![json!({ "score_state": "SCORED", "score": { "strain": 11.37 } })];
        let sleeps = vec![
            json!({ "nap": true, "score_state": "SCORED", "score": { "sleep_performance_percentage": 20.0 } }),
            json!({ "nap": false, "end": "2026-10-20T05:30:00Z", "score_state": "SCORED", "score": {
                "sleep_performance_percentage": 88.0,
                "stage_summary": { "total_light_sleep_time_milli": 10_800_000, "total_rem_sleep_time_milli": 7_200_000, "total_slow_wave_sleep_time_milli": 5_400_000 }
            } }),
        ];
        let workouts = vec![json!({ "sport_name": "Running", "end": "2026-10-19T18:00:00Z", "score_state": "SCORED", "score": { "strain": 9.8 } })];
        let d = summarize(&recoveries, &cycles, &sleeps, &workouts);
        assert_eq!(d["recovery"]["score"], 78.0);
        assert_eq!(d["recovery"]["id"], "8");
        assert_eq!(d["strain"], 11.37);
        assert_eq!(d["sleep"]["performance"], 88.0);
        assert_eq!(d["sleep"]["hours"], 6.5);
        assert_eq!(d["workout"]["sport"], "Running");
        let empty = summarize(&[], &[], &[], &[]);
        assert!(empty["recovery"].is_null() && empty["sleep"].is_null());
    }
}
