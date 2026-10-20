// Home Assistant — the Rust side of the `integration_homeassistant` pill.
//
// Up to four instances ("slots"), each with its address, a long-lived access
// token and the entity ids to watch, all in the Credential Manager / Secret
// Service like the other integrations (secrets.rs). Each configured slot keeps
// one WebSocket open to `/api/websocket`: it authenticates, loads the states
// and the house's config once, then subscribes to `state_changed` and to the
// persistent notifications, which are not entities any more and exist only on
// the WebSocket. The island gets one `integration` event with every instance's
// snapshot, coalesced to a few per second at most, and a sound and a badge when
// an alarm goes off, an update that was not waiting before appears, or a
// notification arrives. The first snapshot of a connection is quiet, like the
// other pollers' first poll.
//
// Writes go through REST, one at a time, from a click on the card: a toggle or
// a scene from a short list, and dismissing a notification. Nothing else.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tauri::AppHandle;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::version::{TLS12, TLS13};
use tokio_rustls::rustls::{crypto::ring as ring_provider, ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::integrations::{self, IntegrationEvent, IntegrationUpdate, PAUSED};
use crate::secrets;

pub const PILL: &str = "integration_homeassistant";
/// How many instances Settings → Integrations offers.
pub const SLOTS: usize = 4;

/// `(domain, service)` pairs the card may call, each on an entity of its own
/// domain. Toggles and scenes only: nothing that arms, unlocks or opens.
const SERVICES: [(&str, &str); 5] = [
    ("light", "toggle"),
    ("switch", "toggle"),
    ("input_boolean", "toggle"),
    ("fan", "toggle"),
    ("scene", "turn_on"),
];

/// Entities listed in Settings beyond this many are ignored.
const MAX_WATCHED: usize = 12;
/// Snapshots are sent to the island at most this often per instance.
const COALESCE: Duration = Duration::from_millis(400);
/// Without any frame from Home Assistant for this long, the socket is reopened.
const SILENCE: Duration = Duration::from_secs(90);
const PING_EVERY: Duration = Duration::from_secs(30);
const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(60);

// ── Settings ──────────────────────────────────────────────────────────────────

/// The Credential Manager key of `what` ("url", "token", "entities") for a slot:
/// the first slot keeps the keys the pill shipped with.
pub fn key(slot: usize, what: &str) -> String {
    if slot <= 1 { format!("homeassistant-{what}") } else { format!("homeassistant-{slot}-{what}") }
}

/// Every key the pill may store, for secrets::KNOWN_KEYS.
pub fn known_keys() -> Vec<String> {
    (1..=SLOTS).flat_map(|slot| ["url", "token", "entities"].map(|what| key(slot, what))).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub slot: usize,
    /// The address as stored, trimmed, without a trailing slash: http(s) only.
    pub base: String,
    pub token: String,
    pub watched: Vec<String>,
}

/// The address as stored, only if it is http(s) and has a host.
pub fn base_url(raw: &str) -> Option<String> {
    let base = raw.trim().trim_end_matches('/').to_string();
    let rest = base.strip_prefix("http://").or_else(|| base.strip_prefix("https://"))?;
    let host = rest.split('/').next().unwrap_or("");
    (!host.is_empty() && !host.contains('@')).then_some(base)
}

/// An entity id as Home Assistant spells them: `domain.object_id`, lowercase
/// letters, digits and underscores. Anything else is refused before it reaches
/// a URL or a request body.
pub fn valid_entity_id(id: &str) -> bool {
    let Some((domain, object)) = id.split_once('.') else { return false };
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    ok(domain) && ok(object)
}

/// The entity ids of a Settings field, one per line, comma or space, invalid ones dropped.
pub fn parse_watched(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        let id = part.trim();
        if valid_entity_id(id) && !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
        if out.len() == MAX_WATCHED {
            break;
        }
    }
    out
}

fn config(slot: usize) -> Option<Config> {
    let base = base_url(&secrets::get(&key(slot, "url"))?)?;
    let token = secrets::get(&key(slot, "token"))?;
    let watched = parse_watched(&secrets::get(&key(slot, "entities")).unwrap_or_default());
    Some(Config { slot, base, token, watched })
}

// ── What each instance knows ──────────────────────────────────────────────────

#[derive(Default)]
struct Instance {
    base: String,
    name: Option<String>,
    version: Option<String>,
    connected: bool,
    error: Option<String>,
    /// entity_id → the state object Home Assistant sent (state + attributes).
    states: HashMap<String, Value>,
    /// notification id → the notification (title, message, created_at).
    notifications: BTreeMap<String, Value>,
    /// What the last snapshot announced, so a change can be told from a first
    /// load: `None` until the first snapshot after a connection.
    seen: Option<Seen>,
}

#[derive(Default, Clone)]
struct Seen {
    triggered: HashSet<String>,
    updates: HashSet<String>,
    notifications: HashSet<String>,
}

static INSTANCES: LazyLock<Mutex<Vec<Instance>>> =
    LazyLock::new(|| Mutex::new((0..SLOTS).map(|_| Instance::default()).collect()));

/// Woken when a Home Assistant key is saved: every connection reopens with the
/// new settings instead of waiting for its next failure.
static RECONFIGURED: LazyLock<Arc<Notify>> = LazyLock::new(|| Arc::new(Notify::new()));

pub fn config_changed() {
    RECONFIGURED.notify_waiters();
}

fn attr<'a>(entity: &'a Value, name: &str) -> Option<&'a Value> {
    entity.get("attributes").and_then(|a| a.get(name))
}

fn friendly_name(entity: &Value, id: &str) -> String {
    attr(entity, "friendly_name").and_then(Value::as_str).unwrap_or(id).to_string()
}

/// One instance's card data, and what in it is new since the last snapshot.
struct Snapshot {
    data: Value,
    event: Option<IntegrationEvent>,
}

fn snapshot(instance: &mut Instance, slot: usize, watched: &[String]) -> Snapshot {
    let mut alarms: Vec<Value> = Vec::new();
    let mut updates: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut lights_on = 0u32;
    let mut rows: Vec<(usize, Value)> = Vec::new();
    let mut triggered: HashSet<String> = HashSet::new();

    let mut ids: Vec<&String> = instance.states.keys().collect();
    ids.sort();
    for id in ids {
        let entity = &instance.states[id];
        let state = entity.get("state").and_then(Value::as_str).unwrap_or("");
        let domain = id.split('.').next().unwrap_or("");
        let name = friendly_name(entity, id);
        match domain {
            "alarm_control_panel" => {
                if state == "triggered" {
                    triggered.insert(id.clone());
                }
                alarms.push(json!({ "entityId": id, "name": name, "state": state }));
            }
            "update" if state == "on" => {
                updates.push(attr(entity, "title").and_then(Value::as_str).map(str::to_string).unwrap_or(name.clone()));
            }
            "binary_sensor" if state == "on" && attr(entity, "device_class").and_then(Value::as_str) == Some("problem") => {
                problems.push(name.clone());
            }
            "light" if state == "on" => lights_on += 1,
            _ => {}
        }
        if let Some(position) = watched.iter().position(|w| w == id) {
            rows.push((position, json!({
                "entityId": id,
                "name": name,
                "state": state,
                "unit": attr(entity, "unit_of_measurement").and_then(Value::as_str),
                "domain": domain,
                // The card shows a button only for a service the list above allows.
                "action": SERVICES.iter().find(|(d, _)| *d == domain).map(|(_, s)| *s),
            })));
        }
    }
    // Settings order, not Home Assistant's.
    rows.sort_by_key(|(position, _)| *position);
    updates.sort();
    updates.dedup();

    let notifications: Vec<Value> = instance
        .notifications
        .iter()
        .map(|(id, n)| {
            json!({
                "id": id,
                "title": n.get("title").and_then(Value::as_str),
                "message": n.get("message").and_then(Value::as_str),
                "createdAt": n.get("created_at").and_then(Value::as_str),
            })
        })
        .collect();

    // Three things worth a sound: an alarm that just went off, an update that
    // was not waiting before, a notification that was not there before. The
    // first snapshot after a connection fills the card quietly.
    let now = Seen {
        triggered: triggered.clone(),
        updates: updates.iter().cloned().collect(),
        notifications: instance.notifications.keys().cloned().collect(),
    };
    let mut event = None;
    if let Some(before) = &instance.seen {
        let mut new_alarm: Vec<String> = instance
            .states
            .iter()
            .filter(|(id, _)| now.triggered.contains(*id) && !before.triggered.contains(*id))
            .map(|(id, e)| friendly_name(e, id))
            .collect();
        new_alarm.sort();
        let new_notifications: Vec<String> = instance
            .notifications
            .iter()
            .filter(|(id, _)| !before.notifications.contains(*id))
            .map(|(_, n)| {
                n.get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::i18n::t("Notification"))
            })
            .collect();
        let new_updates: Vec<&String> = updates.iter().filter(|u| !before.updates.contains(*u)).collect();
        if !new_alarm.is_empty() {
            event = Some(IntegrationEvent {
                success: false,
                label: crate::i18n::t("Alarm triggered"),
                detail: Some(new_alarm.join(", ")),
            });
        } else if !new_notifications.is_empty() {
            let message = instance
                .notifications
                .iter()
                .filter(|(id, _)| !before.notifications.contains(*id))
                .filter_map(|(_, n)| n.get("message").and_then(Value::as_str))
                .next()
                .map(|m| m.chars().take(120).collect::<String>());
            event = Some(IntegrationEvent {
                success: true,
                label: new_notifications.join(", "),
                detail: message,
            });
        } else if !new_updates.is_empty() {
            event = Some(IntegrationEvent {
                success: true,
                label: crate::i18n::t("Update available"),
                detail: Some(new_updates.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")),
            });
        }
    }
    instance.seen = Some(now);

    Snapshot {
        data: json!({
            "slot": slot,
            "url": instance.base,
            "name": instance.name,
            "version": instance.version,
            "connected": instance.connected,
            "error": instance.error,
            "alarms": alarms,
            "updates": updates,
            "problems": problems,
            "lightsOn": lights_on,
            "watched": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
            "notifications": notifications,
        }),
        event,
    }
}

/// Sends the island every configured instance; `slot` is the one whose change
/// may carry the event (the others are only redrawn).
fn emit(app: &AppHandle, slot: usize) {
    let configs: Vec<Config> = (1..=SLOTS).filter_map(config).collect();
    if configs.is_empty() {
        return;
    }
    let mut instances = INSTANCES.lock().unwrap();
    let mut list: Vec<Value> = Vec::new();
    let mut event = None;
    let mut error = None;
    for cfg in &configs {
        let instance = &mut instances[cfg.slot - 1];
        let snap = snapshot(instance, cfg.slot, &cfg.watched);
        if cfg.slot == slot {
            event = snap.event;
        }
        // A single instance that cannot be reached is the card's error line,
        // as for the other pollers; with several, each section says its own.
        if configs.len() == 1 && !instance.connected {
            error = instance.error.clone();
        }
        list.push(snap.data);
    }
    drop(instances);
    integrations::emit(app, IntegrationUpdate { id: PILL, data: json!({ "instances": list }), error, event });
}

// ── The WebSocket ─────────────────────────────────────────────────────────────

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}
type Ws = WebSocketStream<Box<dyn Io>>;

#[derive(Debug, PartialEq)]
enum Failure {
    /// The address could not be reached or the socket broke.
    Unreachable(String),
    /// Home Assistant refused the token.
    Refused,
}

/// `(tls, host, port, path)` of a WebSocket URL derived from the stored address.
pub fn endpoint(base: &str) -> Option<(bool, String, u16, String)> {
    let (tls, rest) = if let Some(r) = base.strip_prefix("https://") {
        (true, r)
    } else {
        (false, base.strip_prefix("http://")?)
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = if let Some(end) = authority.strip_prefix('[').and_then(|a| a.find(']')) {
        let host = &authority[..end + 2];
        let port = authority[end + 2..].strip_prefix(':').and_then(|p| p.parse().ok());
        (host.to_string(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host.to_string(), port.parse().ok())
    } else {
        (authority.to_string(), None)
    };
    if host.is_empty() {
        return None;
    }
    let port = port.unwrap_or(if tls { 443 } else { 80 });
    Some((tls, host, port, format!("{}/api/websocket", path.trim_end_matches('/'))))
}

fn tls_config() -> Arc<ClientConfig> {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_provider(Arc::new(ring_provider::default_provider()))
        .with_protocol_versions(&[&TLS13, &TLS12])
        .expect("the ring provider supports TLS 1.2 and 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

async fn connect(cfg: &Config) -> Result<Ws, Failure> {
    let (tls, host, port, path) = endpoint(&cfg.base).ok_or_else(|| Failure::Unreachable("bad address".into()))?;
    let attempt = async {
        let tcp = TcpStream::connect((host.trim_matches(['[', ']']), port))
            .await
            .map_err(|e| Failure::Unreachable(e.to_string()))?;
        let _ = tcp.set_nodelay(true);
        let stream: Box<dyn Io> = if tls {
            let name = ServerName::try_from(host.clone()).map_err(|e| Failure::Unreachable(e.to_string()))?;
            Box::new(TlsConnector::from(tls_config()).connect(name, tcp).await.map_err(|e| Failure::Unreachable(e.to_string()))?)
        } else {
            Box::new(tcp)
        };
        let url = format!("{}://{}:{}{}", if tls { "wss" } else { "ws" }, host, port, path);
        let request = url.into_client_request().map_err(|e| Failure::Unreachable(e.to_string()))?;
        // The states of a big house are a few megabytes.
        let config = WebSocketConfig::default().max_message_size(Some(32 << 20)).max_frame_size(Some(32 << 20));
        match tokio_tungstenite::client_async_with_config(request, stream, Some(config)).await {
            Ok((ws, _)) => Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) if matches!(response.status().as_u16(), 401 | 403) => {
                Err(Failure::Refused)
            }
            Err(e) => Err(Failure::Unreachable(e.to_string())),
        }
    };
    tokio::time::timeout(Duration::from_secs(15), attempt)
        .await
        .unwrap_or_else(|_| Err(Failure::Unreachable("timeout".into())))
}

async fn send(ws: &mut Ws, message: Value) -> Result<(), Failure> {
    ws.send(Message::Text(message.to_string().into())).await.map_err(|e| Failure::Unreachable(e.to_string()))
}

async fn next_json(ws: &mut Ws) -> Result<Option<Value>, Failure> {
    loop {
        match tokio::time::timeout(SILENCE, ws.next()).await {
            Err(_) => return Err(Failure::Unreachable("no answer".into())),
            Ok(None) => return Ok(None),
            Ok(Some(Err(e))) => return Err(Failure::Unreachable(e.to_string())),
            Ok(Some(Ok(Message::Text(text)))) => {
                return Ok(Some(serde_json::from_str(text.as_str()).unwrap_or(Value::Null)));
            }
            Ok(Some(Ok(Message::Close(_)))) => return Ok(None),
            Ok(Some(Ok(_))) => continue, // ping, pong, binary: nothing to read
        }
    }
}

/// Opens the socket, authenticates, loads and subscribes, then feeds the
/// instance until the socket closes, the settings change, or the pill is
/// paused or switched off. `Ok(())` is a clean end; the caller reconnects.
async fn session(app: &AppHandle, cfg: &Config) -> Result<(), Failure> {
    let mut ws = connect(cfg).await?;

    // auth_required → auth → auth_ok
    match next_json(&mut ws).await? {
        Some(v) if v.get("type").and_then(Value::as_str) == Some("auth_required") => {}
        _ => return Err(Failure::Unreachable("not a Home Assistant WebSocket".into())),
    }
    send(&mut ws, json!({ "type": "auth", "access_token": cfg.token })).await?;
    match next_json(&mut ws).await? {
        Some(v) if v.get("type").and_then(Value::as_str) == Some("auth_ok") => {}
        Some(v) if v.get("type").and_then(Value::as_str) == Some("auth_invalid") => return Err(Failure::Refused),
        _ => return Err(Failure::Unreachable("no auth_ok".into())),
    }

    const CONFIG: u64 = 1;
    const STATES: u64 = 2;
    const STATE_CHANGED: u64 = 3;
    const NOTIFICATIONS: u64 = 4;
    send(&mut ws, json!({ "id": CONFIG, "type": "get_config" })).await?;
    send(&mut ws, json!({ "id": STATES, "type": "get_states" })).await?;
    send(&mut ws, json!({ "id": STATE_CHANGED, "type": "subscribe_events", "event_type": "state_changed" })).await?;
    send(&mut ws, json!({ "id": NOTIFICATIONS, "type": "persistent_notification/subscribe" })).await?;
    let mut next_id = NOTIFICATIONS + 1;

    {
        let mut instances = INSTANCES.lock().unwrap();
        let instance = &mut instances[cfg.slot - 1];
        instance.base = cfg.base.clone();
        instance.connected = true;
        instance.error = None;
        instance.states.clear();
        instance.notifications.clear();
        instance.seen = None;
    }

    let mut dirty = false;
    let mut loaded = false; // states and notifications both in: the first snapshot can go
    let mut have_states = false;
    let mut have_notifications = false;
    let mut flush_at: Option<Instant> = None;
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await; // the first tick is immediate
    let reconfigured = RECONFIGURED.clone();
    let notified = reconfigured.notified();
    tokio::pin!(notified);

    loop {
        let deadline = flush_at;
        let flush = async move {
            match deadline {
                Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                None => std::future::pending::<()>().await,
            }
        };
        let mut do_ping = false;
        tokio::select! {
            _ = &mut notified => return Ok(()),
            _ = ping.tick() => {
                if PAUSED.load(Ordering::Relaxed) || !integrations::enabled(app, PILL) {
                    return Ok(());
                }
                do_ping = true;
            }
            _ = flush => {
                flush_at = None;
                if dirty && loaded {
                    dirty = false;
                    emit(app, cfg.slot);
                }
            }
            frame = next_json(&mut ws) => {
                let Some(message) = frame? else { return Ok(()) };
                let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
                let id = message.get("id").and_then(Value::as_u64).unwrap_or(0);
                {
                let mut instances = INSTANCES.lock().unwrap();
                let instance = &mut instances[cfg.slot - 1];
                match (kind, id) {
                    ("result", CONFIG) => {
                        if let Some(result) = message.get("result") {
                            instance.name = result.get("location_name").and_then(Value::as_str).map(str::to_string);
                            instance.version = result.get("version").and_then(Value::as_str).map(str::to_string);
                            dirty = true;
                        }
                    }
                    ("result", STATES) => {
                        if let Some(list) = message.get("result").and_then(Value::as_array) {
                            instance.states.clear();
                            for entity in list {
                                if let Some(id) = entity.get("entity_id").and_then(Value::as_str) {
                                    instance.states.insert(id.to_string(), entity.clone());
                                }
                            }
                            have_states = true;
                            dirty = true;
                        }
                    }
                    ("result", _) => {
                        if message.get("success") == Some(&Value::Bool(false)) {
                            crate::log::line(format!("Home Assistant {}: command {id} failed", cfg.slot));
                        }
                    }
                    ("event", STATE_CHANGED) => {
                        let data = message.get("event").and_then(|e| e.get("data"));
                        if let Some(id) = data.and_then(|d| d.get("entity_id")).and_then(Value::as_str) {
                            match data.and_then(|d| d.get("new_state")) {
                                Some(Value::Null) | None => { instance.states.remove(id); }
                                Some(state) => { instance.states.insert(id.to_string(), state.clone()); }
                            }
                            dirty = true;
                        }
                    }
                    ("event", NOTIFICATIONS) => {
                        let event = message.get("event");
                        let change = event.and_then(|e| e.get("type")).and_then(Value::as_str).unwrap_or("");
                        let items = event.and_then(|e| e.get("notifications")).and_then(Value::as_object);
                        match change {
                            "current" => {
                                instance.notifications.clear();
                                if let Some(items) = items {
                                    for (id, n) in items {
                                        instance.notifications.insert(id.clone(), n.clone());
                                    }
                                }
                                have_notifications = true;
                            }
                            "added" | "updated" => {
                                if let Some(items) = items {
                                    for (id, n) in items {
                                        instance.notifications.insert(id.clone(), n.clone());
                                    }
                                }
                            }
                            "removed" => {
                                if let Some(items) = items {
                                    for id in items.keys() {
                                        instance.notifications.remove(id);
                                    }
                                }
                            }
                            _ => {}
                        }
                        dirty = true;
                    }
                    _ => {}
                }
                }
                loaded = have_states && have_notifications;
                if dirty && loaded && flush_at.is_none() {
                    flush_at = Some(Instant::now() + COALESCE);
                }
            }
        }
        if do_ping {
            send(&mut ws, json!({ "id": next_id, "type": "ping" })).await?;
            next_id += 1;
        }
    }
}

async fn run_slot(app: AppHandle, slot: usize) {
    let mut delay = RETRY_MIN;
    loop {
        let cfg = if PAUSED.load(Ordering::Relaxed) || !integrations::enabled(&app, PILL) { None } else { config(slot) };
        let Some(cfg) = cfg else {
            // Off, paused or not set up: nothing reaches the network. Wake up
            // early if the settings change meanwhile.
            let _ = tokio::time::timeout(RETRY_MIN, RECONFIGURED.notified()).await;
            continue;
        };
        let outcome = session(&app, &cfg).await;
        // The guard lives in its own block: the task must stay `Send`, so no
        // lock is held across the waits below.
        let was_connected = {
            let mut instances = INSTANCES.lock().unwrap();
            let instance = &mut instances[slot - 1];
            let was_connected = instance.connected;
            instance.connected = false;
            match &outcome {
                Ok(()) => {
                    instance.error = None;
                    delay = RETRY_MIN;
                }
                Err(Failure::Refused) => {
                    instance.error = Some(crate::i18n::t("Token lacks access"));
                    delay = RETRY_MAX;
                }
                Err(Failure::Unreachable(why)) => {
                    crate::log::line(format!("Home Assistant {slot}: {why}"));
                    instance.error = Some(crate::i18n::tf("No connection: {error}", &[("error", why.as_str())]));
                }
            }
            was_connected
        };
        if was_connected || outcome.is_err() {
            emit(&app, slot);
        }
        if outcome.is_ok() {
            // A clean end: the settings changed or the pill was switched off.
            continue;
        }
        let _ = tokio::time::timeout(delay, RECONFIGURED.notified()).await;
        delay = (delay * 2).min(RETRY_MAX);
    }
}

/// One task per slot, for the life of the app.
pub fn start(app: AppHandle) {
    for slot in 1..=SLOTS {
        tauri::async_runtime::spawn(run_slot(app.clone(), slot));
    }
}

// ── Writes, from a click on the card ─────────────────────────────────────────

async fn post(cfg: &Config, path: &str, body: Value) -> Result<(), String> {
    let response = integrations::client()
        .post(format!("{}{path}", cfg.base))
        .header("Authorization", format!("Bearer {}", cfg.token))
        .json(&body)
        .send()
        .await
        .map_err(|e| crate::i18n::tf("No connection: {error}", &[("error", &e.to_string())]))?;
    if !response.status().is_success() {
        return Err(integrations::status_error(response.status().as_u16(), &crate::i18n::t("Token lacks access")));
    }
    Ok(())
}

/// `domain.service` on one entity of that domain, from the short list in
/// `SERVICES`. The state that follows arrives on the socket, so the card shows
/// what Home Assistant says happened, not what was asked.
pub async fn call_service(slot: usize, domain: String, service: String, entity_id: String) -> Result<(), String> {
    if !SERVICES.iter().any(|(d, s)| *d == domain && *s == service) {
        return Err(format!("{domain}.{service} is not a service the card may call"));
    }
    if !valid_entity_id(&entity_id) || !entity_id.starts_with(&format!("{domain}.")) {
        return Err(format!("{entity_id} is not an entity of {domain}"));
    }
    let cfg = config(slot).ok_or_else(|| crate::i18n::t("Key not configured"))?;
    post(&cfg, &format!("/api/services/{domain}/{service}"), json!({ "entity_id": entity_id })).await
}

/// Dismisses one persistent notification; the subscription reports the removal.
pub async fn dismiss_notification(slot: usize, id: String) -> Result<(), String> {
    if id.is_empty() || id.len() > 200 || id.chars().any(|c| c.is_control()) {
        return Err("bad notification id".into());
    }
    let cfg = config(slot).ok_or_else(|| crate::i18n::t("Key not configured"))?;
    post(&cfg, "/api/services/persistent_notification/dismiss", json!({ "notification_id": id })).await
}

/// Opens the instance in the browser — the address lives in the Credential
/// Manager, so the island cannot read it itself.
pub fn open(slot: usize) {
    if let Some(cfg) = config(slot) {
        crate::platform::open_url(&cfg.base);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_keep_the_first_slot_as_shipped() {
        assert_eq!(key(1, "url"), "homeassistant-url");
        assert_eq!(key(2, "token"), "homeassistant-2-token");
        assert_eq!(known_keys().len(), SLOTS * 3);
    }

    #[test]
    fn addresses_are_http_only_and_trimmed() {
        assert_eq!(base_url(" http://homeassistant.local:8123/ ").as_deref(), Some("http://homeassistant.local:8123"));
        assert_eq!(base_url("https://ha.example.com"), Some("https://ha.example.com".into()));
        assert_eq!(base_url("homeassistant.local:8123"), None);
        assert_eq!(base_url("http://user:pw@host"), None);
        assert_eq!(base_url("http://"), None);
    }

    #[test]
    fn the_websocket_endpoint_follows_the_address() {
        assert_eq!(endpoint("http://homeassistant.local:8123"), Some((false, "homeassistant.local".into(), 8123, "/api/websocket".into())));
        assert_eq!(endpoint("https://ha.example.com"), Some((true, "ha.example.com".into(), 443, "/api/websocket".into())));
        assert_eq!(endpoint("http://192.168.1.31:8123/"), Some((false, "192.168.1.31".into(), 8123, "/api/websocket".into())));
        assert_eq!(endpoint("https://proxy.example.com/ha"), Some((true, "proxy.example.com".into(), 443, "/ha/api/websocket".into())));
        assert_eq!(endpoint("http://[fe80::1]:8123"), Some((false, "[fe80::1]".into(), 8123, "/api/websocket".into())));
    }

    #[test]
    fn watched_entities_are_validated_and_capped() {
        let parsed = parse_watched("light.kitchen, sensor.power\nscene.movie; Light.Bad light.kitchen rm -rf");
        assert_eq!(parsed, vec!["light.kitchen", "sensor.power", "scene.movie"]);
        let many: Vec<String> = (0..20).map(|i| format!("sensor.s{i}")).collect();
        assert_eq!(parse_watched(&many.join(",")).len(), MAX_WATCHED);
    }

    fn entity(id: &str, state: &str, attrs: Value) -> (String, Value) {
        (id.to_string(), json!({ "entity_id": id, "state": state, "attributes": attrs }))
    }

    #[test]
    fn a_snapshot_lists_what_the_card_shows_and_is_quiet_the_first_time() {
        let mut instance = Instance { base: "http://ha:8123".into(), name: Some("Casa".into()), connected: true, ..Default::default() };
        instance.states.extend([
            entity("alarm_control_panel.home", "disarmed", json!({ "friendly_name": "Home" })),
            entity("update.core", "on", json!({ "title": "Core 2026.10", "friendly_name": "Core" })),
            entity("binary_sensor.boiler", "on", json!({ "device_class": "problem", "friendly_name": "Boiler" })),
            entity("light.kitchen", "on", json!({ "friendly_name": "Kitchen" })),
            entity("light.hall", "off", json!({})),
            entity("sensor.power", "412", json!({ "unit_of_measurement": "W", "friendly_name": "Power" })),
        ]);
        instance.notifications.insert("n1".into(), json!({ "title": "Backup done", "message": "ok", "created_at": "2026-10-20T06:00:00Z" }));
        let watched = vec!["sensor.power".to_string(), "light.kitchen".to_string()];

        let first = snapshot(&mut instance, 1, &watched);
        assert!(first.event.is_none(), "the first snapshot is quiet");
        let d = &first.data;
        assert_eq!(d["name"], "Casa");
        assert_eq!(d["lightsOn"], 1);
        assert_eq!(d["updates"], json!(["Core 2026.10"]));
        assert_eq!(d["problems"], json!(["Boiler"]));
        assert_eq!(d["alarms"][0]["state"], "disarmed");
        assert_eq!(d["watched"][0]["entityId"], "sensor.power");
        assert_eq!(d["watched"][0]["unit"], "W");
        assert_eq!(d["watched"][0]["action"], Value::Null);
        assert_eq!(d["watched"][1]["action"], "toggle");
        assert_eq!(d["notifications"][0]["title"], "Backup done");

        // An alarm going off beats everything else.
        instance.states.insert("alarm_control_panel.home".into(), entity("alarm_control_panel.home", "triggered", json!({ "friendly_name": "Home" })).1);
        instance.notifications.insert("n2".into(), json!({ "title": "Door", "message": "open" }));
        let second = snapshot(&mut instance, 1, &watched);
        let event = second.event.expect("an alarm event");
        assert!(!event.success);
        assert_eq!(event.detail.as_deref(), Some("Home"));

        // Nothing new: nothing said.
        assert!(snapshot(&mut instance, 1, &watched).event.is_none());

        // A notification alone is a happy event with its title and message.
        instance.notifications.insert("n3".into(), json!({ "title": "Washer", "message": "Cycle finished" }));
        let third = snapshot(&mut instance, 1, &watched);
        let event = third.event.expect("a notification event");
        assert!(event.success);
        assert_eq!(event.label, "Washer");
        assert_eq!(event.detail.as_deref(), Some("Cycle finished"));
    }

    #[test]
    fn only_the_short_list_of_services_is_accepted() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        assert!(rt.block_on(call_service(1, "lock".into(), "unlock".into(), "lock.door".into())).is_err());
        assert!(rt.block_on(call_service(1, "light".into(), "toggle".into(), "switch.plug".into())).is_err());
        assert!(rt.block_on(call_service(1, "light".into(), "toggle".into(), "light.ok; rm".into())).is_err());
        assert!(rt.block_on(dismiss_notification(1, "".into())).is_err());
    }
}
