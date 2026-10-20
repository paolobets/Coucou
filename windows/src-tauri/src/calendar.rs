// Calendar — the Rust side of the `integration_calendar` pill.
//
// Up to four calendars by their iCal address (the "secret address in iCal
// format" of Google Calendar, the published .ics of Outlook, a shared iCloud
// calendar, any webcal): no sign-in, no key, the link is the secret and lives
// in the Credential Manager / Secret Service like the other integrations.
//
// Every ten minutes each feed is fetched and parsed here, with its recurrence
// rules expanded for the next two days: DAILY, WEEKLY (BYDAY), MONTHLY
// (BYMONTHDAY, or an "nth weekday" BYDAY) and YEARLY, with INTERVAL, COUNT,
// UNTIL, EXDATE, and the RECURRENCE-ID overrides a moved instance leaves.
// Times with a Z are UTC; floating times and TZID times are read as the
// computer's local time, which is right for one's own calendars (a feed from
// another time zone would be off by the difference: no time-zone database
// ships with Coucou). The card gets today's and tomorrow's events; ten
// minutes before one starts, the pill badges and Mochi says its name.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::integrations::{self, IntegrationEvent, IntegrationUpdate};
use crate::secrets;

pub const PILL: &str = "integration_calendar";
pub const SLOTS: usize = 4;
/// Feeds are fetched again after this long; the poll itself runs every minute
/// for the reminders.
const FETCH_EVERY_SECS: u64 = 600;
/// How far ahead the card looks.
const AHEAD_SECS: i64 = 2 * 86_400;
/// The reminder fires when an event starts within this many seconds.
const REMIND_SECS: i64 = 10 * 60;
/// A feed bigger than this is not parsed (a whole Google history is a few MB).
const MAX_FEED_BYTES: usize = 24 << 20;
/// No recurrence is expanded past this many instances.
const MAX_INSTANCES: usize = 2000;

pub fn key(slot: usize) -> String {
    if slot <= 1 { "calendar-url".to_string() } else { format!("calendar-{slot}-url") }
}

pub fn known_keys() -> Vec<String> {
    (1..=SLOTS).map(key).collect()
}

fn feed_url(slot: usize) -> Option<String> {
    let raw = secrets::get(&key(slot))?;
    let url = raw.trim();
    let url = url.strip_prefix("webcal://").map(|rest| format!("https://{rest}")).unwrap_or_else(|| url.to_string());
    (url.starts_with("https://") || url.starts_with("http://")).then_some(url)
}

// ── Civil dates (proleptic Gregorian, days since 1970-01-01) ─────────────────

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 0 = Monday … 6 = Sunday.
fn weekday(days: i64) -> u32 {
    ((days + 3).rem_euclid(7)) as u32
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 29 } else { 28 },
    }
}

/// Seconds east of UTC of the computer's clock, from its local time and the
/// UTC time read together (rounded to a quarter of an hour).
pub fn local_offset_secs() -> i64 {
    let utc = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let t = crate::platform::local_time();
    let local = days_from_civil(t.year as i64, t.month, t.day) * 86_400 + t.hour as i64 * 3600 + t.minute as i64 * 60 + t.second as i64;
    ((local - utc) as f64 / 900.0).round() as i64 * 900
}

// ── iCalendar ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub uid: String,
    pub title: String,
    pub location: Option<String>,
    /// Unix seconds. An all-day event starts at the local midnight.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub calendar: usize,
}

/// A property line split into its name, its parameters and its value.
struct Prop<'a> {
    name: &'a str,
    params: Vec<(&'a str, &'a str)>,
    value: &'a str,
}

fn parse_prop(line: &str) -> Option<Prop<'_>> {
    // The value starts at the first ':' outside a quoted parameter.
    let mut in_quotes = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let name = parts.next()?;
    let params = parts
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k, v.trim_matches('"')))
        .collect();
    Some(Prop { name, params, value })
}

/// Lines, unfolded: a line starting with a space or a tab continues the one before.
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')) {
            if let Some(last) = out.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        out.push(line.to_string());
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A date or date-time value: `(unix seconds, all_day)`. Z means UTC; a date
/// or a floating/TZID time is local, placed with `offset`.
fn parse_time(value: &str, params: &[(&str, &str)], offset: i64) -> Option<(i64, bool)> {
    let value = value.trim();
    let is_date = params.iter().any(|(k, v)| *k == "VALUE" && *v == "DATE") || value.len() == 8;
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if is_date {
        if value.len() != 8 || !digits(value) {
            return None;
        }
        let (y, m, d) = (value[..4].parse().ok()?, value[4..6].parse().ok()?, value[6..8].parse().ok()?);
        return Some((days_from_civil(y, m, d) * 86_400 - offset, true));
    }
    let (date, time) = value.split_once('T')?;
    let utc = time.ends_with('Z');
    let time = time.trim_end_matches('Z');
    if date.len() != 8 || time.len() < 4 || !digits(date) || !digits(time) {
        return None;
    }
    let (y, m, d) = (date[..4].parse().ok()?, date[4..6].parse().ok()?, date[6..8].parse().ok()?);
    let (hh, mm): (i64, i64) = (time[..2].parse().ok()?, time[2..4].parse().ok()?);
    let ss: i64 = if time.len() >= 6 { time[4..6].parse().ok()? } else { 0 };
    let tz_utc = params.iter().any(|(k, v)| *k == "TZID" && (v.eq_ignore_ascii_case("UTC") || v.eq_ignore_ascii_case("Etc/UTC")));
    let secs = days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss;
    Some((if utc || tz_utc { secs } else { secs - offset }, false))
}

/// `PnDTnHnMnS` / `PnW`, in seconds.
fn parse_duration(value: &str) -> Option<i64> {
    let s = value.trim().trim_start_matches('+');
    let (sign, s) = match s.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, s),
    };
    let s = s.strip_prefix('P')?;
    let mut total = 0i64;
    let mut n = String::new();
    for c in s.chars() {
        match c {
            '0'..='9' => n.push(c),
            'T' => {}
            'W' | 'D' | 'H' | 'M' | 'S' => {
                let v: i64 = n.parse().ok()?;
                n.clear();
                total += v * match c {
                    'W' => 7 * 86_400,
                    'D' => 86_400,
                    'H' => 3600,
                    'M' => 60,
                    _ => 1,
                };
            }
            _ => return None,
        }
    }
    Some(sign * total)
}

#[derive(Debug, Clone, PartialEq)]
struct Rule {
    freq: String,
    interval: i64,
    count: Option<usize>,
    until: Option<i64>,
    /// Weekdays (0 = Monday), with an optional ordinal for MONTHLY ("2TU" → (1, Some(2))).
    by_day: Vec<(u32, Option<i32>)>,
    by_month_day: Vec<i32>,
}

fn parse_weekday(s: &str) -> Option<u32> {
    Some(match s {
        "MO" => 0,
        "TU" => 1,
        "WE" => 2,
        "TH" => 3,
        "FR" => 4,
        "SA" => 5,
        "SU" => 6,
        _ => return None,
    })
}

fn parse_rule(value: &str, offset: i64) -> Option<Rule> {
    let mut rule = Rule { freq: String::new(), interval: 1, count: None, until: None, by_day: Vec::new(), by_month_day: Vec::new() };
    for part in value.split(';') {
        let (k, v) = part.split_once('=')?;
        match k {
            "FREQ" => rule.freq = v.to_string(),
            "INTERVAL" => rule.interval = v.parse().ok().filter(|i| *i >= 1)?,
            "COUNT" => rule.count = Some(v.parse().ok()?),
            "UNTIL" => rule.until = parse_time(v, &[], offset).map(|(t, all_day)| if all_day { t + 86_400 - 1 } else { t }),
            "BYDAY" => {
                for d in v.split(',') {
                    let split = d.len().saturating_sub(2);
                    let (ord, day) = d.split_at(split);
                    let ordinal = if ord.is_empty() { None } else { Some(ord.parse().ok()?) };
                    rule.by_day.push((parse_weekday(day)?, ordinal));
                }
            }
            "BYMONTHDAY" => rule.by_month_day = v.split(',').filter_map(|d| d.parse().ok()).collect(),
            _ => {} // WKST, BYMONTH, BYSETPOS…: left as the master's own day
        }
    }
    (!rule.freq.is_empty()).then_some(rule)
}

/// The days (since 1970) of the n-th `weekday` of a month, or the last for -1.
fn nth_weekday(y: i64, m: u32, wd: u32, n: i32) -> Option<i64> {
    let first = days_from_civil(y, m, 1);
    let len = days_in_month(y, m) as i64;
    if n > 0 {
        let delta = (wd as i64 - weekday(first) as i64).rem_euclid(7);
        let day = first + delta + (n as i64 - 1) * 7;
        (day < first + len).then_some(day)
    } else {
        let last = first + len - 1;
        let delta = (weekday(last) as i64 - wd as i64).rem_euclid(7);
        let day = last - delta + (n as i64 + 1) * 7;
        (day >= first).then_some(day)
    }
}

/// Every start time of a recurring event from its first one up to `until`
/// (exclusive), in order, COUNT and UNTIL honoured.
fn expand(start: i64, rule: &Rule, until: i64, offset: i64) -> Vec<i64> {
    let mut out = Vec::new();
    let limit = match rule.until {
        Some(u) => until.min(u + 1),
        None => until,
    };
    let count = rule.count.unwrap_or(usize::MAX);
    // Work in local wall-clock days: a 9:00 meeting stays at 9:00.
    let local = start + offset;
    let (day0, tod) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    let push = |out: &mut Vec<i64>, day: i64| -> bool {
        let t = day * 86_400 + tod - offset;
        if t >= limit || out.len() >= count || out.len() >= MAX_INSTANCES {
            return false;
        }
        if t >= start {
            out.push(t);
        }
        true
    };
    match rule.freq.as_str() {
        "DAILY" => {
            let mut day = day0;
            while push(&mut out, day) {
                day += rule.interval;
            }
        }
        "WEEKLY" => {
            let days: Vec<u32> = if rule.by_day.is_empty() { vec![weekday(day0)] } else { rule.by_day.iter().map(|(d, _)| *d).collect() };
            let week_start = day0 - weekday(day0) as i64;
            let mut week = 0i64;
            'weeks: loop {
                let base = week_start + week * 7 * rule.interval;
                let mut sorted = days.clone();
                sorted.sort_unstable();
                for d in sorted {
                    let day = base + d as i64;
                    if day < day0 {
                        continue;
                    }
                    if !push(&mut out, day) {
                        break 'weeks;
                    }
                }
                week += 1;
                if week > 60_000 {
                    break;
                }
            }
        }
        "MONTHLY" => {
            let (y0, m0, d0) = civil_from_days(day0);
            let mut months = 0i64;
            'months: loop {
                let total = y0 * 12 + (m0 as i64 - 1) + months * rule.interval;
                let (y, m) = (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32);
                let mut days: Vec<i64> = Vec::new();
                if !rule.by_month_day.is_empty() {
                    for md in &rule.by_month_day {
                        let len = days_in_month(y, m) as i32;
                        let d = if *md > 0 { *md } else { len + 1 + *md };
                        if d >= 1 && d <= len {
                            days.push(days_from_civil(y, m, d as u32));
                        }
                    }
                } else if !rule.by_day.is_empty() {
                    for (wd, ord) in &rule.by_day {
                        if let Some(day) = nth_weekday(y, m, *wd, ord.unwrap_or(1)) {
                            days.push(day);
                        }
                    }
                } else if d0 <= days_in_month(y, m) {
                    days.push(days_from_civil(y, m, d0));
                }
                days.sort_unstable();
                for day in days {
                    if day < day0 {
                        continue;
                    }
                    if !push(&mut out, day) {
                        break 'months;
                    }
                }
                months += 1;
                if months > 12_000 {
                    break;
                }
            }
        }
        "YEARLY" => {
            let (y0, m0, d0) = civil_from_days(day0);
            let mut years = 0i64;
            loop {
                let y = y0 + years * rule.interval;
                if d0 <= days_in_month(y, m0) && !push(&mut out, days_from_civil(y, m0, d0)) {
                    break;
                }
                years += 1;
                if years > 1000 {
                    break;
                }
            }
        }
        _ => out.push(start),
    }
    out
}

/// The events of one feed that touch `[from, to)`, recurrences expanded,
/// moved and cancelled instances applied, sorted by start.
pub fn parse_ics(text: &str, calendar: usize, from: i64, to: i64, offset: i64) -> Vec<Event> {
    struct Raw {
        uid: String,
        title: String,
        location: Option<String>,
        start: Option<(i64, bool)>,
        end: Option<(i64, bool)>,
        duration: Option<i64>,
        rule: Option<Rule>,
        exdates: Vec<i64>,
        recurrence_id: Option<i64>,
        status_cancelled: bool,
    }
    let mut raws: Vec<Raw> = Vec::new();
    let mut current: Option<Raw> = None;
    for line in unfold(text) {
        if line == "BEGIN:VEVENT" {
            current = Some(Raw {
                uid: String::new(),
                title: String::new(),
                location: None,
                start: None,
                end: None,
                duration: None,
                rule: None,
                exdates: Vec::new(),
                recurrence_id: None,
                status_cancelled: false,
            });
            continue;
        }
        if line == "END:VEVENT" {
            if let Some(raw) = current.take() {
                raws.push(raw);
            }
            continue;
        }
        let Some(raw) = current.as_mut() else { continue };
        let Some(prop) = parse_prop(&line) else { continue };
        match prop.name {
            "UID" => raw.uid = prop.value.to_string(),
            "SUMMARY" => raw.title = unescape(prop.value),
            "LOCATION" => raw.location = Some(unescape(prop.value)).filter(|s| !s.is_empty()),
            "DTSTART" => raw.start = parse_time(prop.value, &prop.params, offset),
            "DTEND" => raw.end = parse_time(prop.value, &prop.params, offset),
            "DURATION" => raw.duration = parse_duration(prop.value),
            "RRULE" => raw.rule = parse_rule(prop.value, offset),
            "EXDATE" => raw.exdates.extend(prop.value.split(',').filter_map(|v| parse_time(v, &prop.params, offset)).map(|(t, _)| t)),
            "RECURRENCE-ID" => raw.recurrence_id = parse_time(prop.value, &prop.params, offset).map(|(t, _)| t),
            "STATUS" => raw.status_cancelled = prop.value.eq_ignore_ascii_case("CANCELLED"),
            _ => {}
        }
    }

    // A moved or cancelled instance replaces the master's one at that time.
    let mut overridden: HashMap<String, HashSet<i64>> = HashMap::new();
    for raw in raws.iter().filter(|r| r.recurrence_id.is_some()) {
        overridden.entry(raw.uid.clone()).or_default().insert(raw.recurrence_id.unwrap());
    }

    let mut out: Vec<Event> = Vec::new();
    for raw in &raws {
        if raw.status_cancelled {
            continue;
        }
        let Some((start, all_day)) = raw.start else { continue };
        let length = match (raw.end, raw.duration) {
            (Some((end, _)), _) => (end - start).max(0),
            (None, Some(d)) => d.max(0),
            (None, None) => if all_day { 86_400 } else { 0 },
        };
        let starts: Vec<i64> = match (&raw.rule, raw.recurrence_id) {
            (Some(rule), None) => expand(start, rule, to, offset)
                .into_iter()
                .filter(|t| !raw.exdates.contains(t))
                .filter(|t| !overridden.get(&raw.uid).is_some_and(|set| set.contains(t)))
                .collect(),
            _ => vec![start],
        };
        for s in starts {
            let e = s + length;
            if e <= from || s >= to {
                continue;
            }
            out.push(Event {
                uid: raw.uid.clone(),
                title: if raw.title.is_empty() { "(untitled)".into() } else { raw.title.clone() },
                location: raw.location.clone(),
                start: s,
                end: e,
                all_day,
                calendar,
            });
        }
    }
    out.sort_by_key(|e| (e.start, e.title.clone()));
    out
}

// ── The poll ──────────────────────────────────────────────────────────────────

struct Cache {
    fetched_at: Option<Instant>,
    events: Vec<Event>,
    errors: Vec<(usize, String)>,
    reminded: HashSet<(String, i64)>,
}

static CACHE: LazyLock<Mutex<Cache>> =
    LazyLock::new(|| Mutex::new(Cache { fetched_at: None, events: Vec::new(), errors: Vec::new(), reminded: HashSet::new() }));

/// Woken when a calendar key is saved: the feeds are read again at once.
pub fn config_changed() {
    CACHE.lock().unwrap().fetched_at = None;
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

async fn fetch(slot: usize, url: &str) -> Result<String, String> {
    let response = integrations::client()
        .get(url)
        .send()
        .await
        .map_err(|e| crate::i18n::tf("No connection: {error}", &[("error", &e.to_string())]))?;
    if !response.status().is_success() {
        return Err(integrations::status_error(response.status().as_u16(), &crate::i18n::t("The link is not valid or has expired")));
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FEED_BYTES {
        return Err(crate::i18n::tf("Calendar {n}: the feed is too large", &[("n", &slot.to_string())]));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub async fn poll(app: AppHandle) {
    let feeds: Vec<(usize, String)> = (1..=SLOTS).filter_map(|s| feed_url(s).map(|u| (s, u))).collect();
    if feeds.is_empty() {
        return;
    }
    let stale = {
        let cache = CACHE.lock().unwrap();
        cache.fetched_at.is_none_or(|t| t.elapsed().as_secs() >= FETCH_EVERY_SECS)
    };
    let now = now_secs();
    if stale {
        let offset = local_offset_secs();
        let (from, to) = (now - 86_400, now + AHEAD_SECS);
        let mut events = Vec::new();
        let mut errors = Vec::new();
        for (slot, url) in &feeds {
            match fetch(*slot, url).await {
                Ok(text) => events.extend(parse_ics(&text, *slot, from, to, offset)),
                Err(e) => errors.push((*slot, e)),
            }
        }
        events.sort_by_key(|e| (e.start, e.title.clone()));
        let mut cache = CACHE.lock().unwrap();
        cache.fetched_at = Some(Instant::now());
        cache.events = events;
        cache.errors = errors;
        cache.reminded.retain(|(_, start)| *start > now - 3600);
    }

    let (upcoming, errors, event) = {
        let mut cache = CACHE.lock().unwrap();
        // Still on or ahead, and nothing before the past hour.
        let upcoming: Vec<Event> = cache.events.iter().filter(|e| e.end > now && e.start < now + AHEAD_SECS).cloned().collect();
        // One reminder per instance, when it starts within ten minutes.
        let mut event = None;
        for e in upcoming.iter().filter(|e| !e.all_day && e.start > now && e.start <= now + REMIND_SECS) {
            let id = (e.uid.clone(), e.start);
            if cache.reminded.insert(id) {
                let minutes = ((e.start - now) as f64 / 60.0).ceil() as i64;
                event = Some(IntegrationEvent {
                    success: true,
                    label: e.title.clone(),
                    detail: Some(crate::i18n::tf("in {n} min", &[("n", &minutes.to_string())])),
                });
                break;
            }
        }
        (upcoming, cache.errors.clone(), event)
    };
    let error = if feeds.len() == errors.len() && !errors.is_empty() { Some(errors[0].1.clone()) } else { None };
    integrations::emit(&app, IntegrationUpdate {
        id: PILL,
        data: json!({
            "now": now,
            "offset": local_offset_secs(),
            "calendars": feeds.len(),
            "events": upcoming.iter().take(40).collect::<Vec<_>>(),
            "errors": errors.iter().map(|(slot, e)| json!({ "calendar": slot, "error": e })).collect::<Vec<Value>>(),
        }),
        error,
        event,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3600;

    fn at(y: i64, m: u32, d: u32, hh: i64, mm: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400 + hh * H + mm * 60
    }

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 10, 20), 20_746);
        assert_eq!(civil_from_days(20_746), (2026, 10, 20));
        assert_eq!(weekday(days_from_civil(2026, 10, 20)), 1, "a Tuesday");
        assert_eq!(days_in_month(2028, 2), 29);
        assert_eq!(nth_weekday(2026, 10, 1, 2), Some(days_from_civil(2026, 10, 13)), "second Tuesday");
        assert_eq!(nth_weekday(2026, 10, 4, -1), Some(days_from_civil(2026, 10, 30)), "last Friday");
    }

    #[test]
    fn times_read_utc_local_and_dates() {
        let offset = 2 * H; // Rome in October
        assert_eq!(parse_time("20261020T143000Z", &[], offset), Some((at(2026, 10, 20, 14, 30), false)));
        assert_eq!(parse_time("20261020T163000", &[("TZID", "Europe/Rome")], offset), Some((at(2026, 10, 20, 14, 30), false)));
        assert_eq!(parse_time("20261020", &[("VALUE", "DATE")], offset), Some((at(2026, 10, 19, 22, 0), true)));
        assert_eq!(parse_duration("PT1H30M"), Some(5400));
        assert_eq!(parse_duration("P1DT2H"), Some(93_600));
        assert_eq!(parse_duration("P2W"), Some(14 * 86_400));
    }

    #[test]
    fn lines_unfold_and_values_unescape() {
        let lines = unfold("SUMMARY:Riunione molto\r\n  lunga\r\nLOCATION:Sala\\, piano 2\r\n");
        assert_eq!(lines[0], "SUMMARY:Riunione molto lunga");
        assert_eq!(unescape("Sala\\, piano 2\\nnote"), "Sala, piano 2\nnote");
        let p = parse_prop("DTSTART;TZID=\"Europe/Rome\":20261020T163000").unwrap();
        assert_eq!((p.name, p.value), ("DTSTART", "20261020T163000"));
        assert_eq!(p.params, vec![("TZID", "Europe/Rome")]);
    }

    const FEED: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\nUID:weekly\r\nSUMMARY:Stand-up\r\nDTSTART;TZID=Europe/Rome:20260105T091500\r\nDTEND;TZID=Europe/Rome:20260105T093000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE,FR\r\nEXDATE;TZID=Europe/Rome:20261021T091500\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:weekly\r\nRECURRENCE-ID;TZID=Europe/Rome:20261023T091500\r\nSUMMARY:Stand-up (spostato)\r\nDTSTART;TZID=Europe/Rome:20261023T110000\r\nDTEND;TZID=Europe/Rome:20261023T111500\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:single\r\nSUMMARY:Dentista\r\nLOCATION:Via Roma 1\r\nDTSTART:20261020T143000Z\r\nDTEND:20261020T153000Z\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:allday\r\nSUMMARY:Compleanno\r\nDTSTART;VALUE=DATE:20261021\r\nDTEND;VALUE=DATE:20261022\r\nRRULE:FREQ=YEARLY\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:monthly\r\nSUMMARY:Bolletta\r\nDTSTART;VALUE=DATE:20260122\r\nRRULE:FREQ=MONTHLY;BYMONTHDAY=22;COUNT=12\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:old\r\nSUMMARY:Vecchio\r\nDTSTART:20250101T100000Z\r\nDTEND:20250101T110000Z\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:gone\r\nSUMMARY:Annullato\r\nSTATUS:CANCELLED\r\nDTSTART:20261020T180000Z\r\nDTEND:20261020T190000Z\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn a_feed_expands_into_the_window_with_exceptions_applied() {
        let offset = 2 * H;
        // Tuesday 20 October 2026, 10:00 Rome, four days ahead.
        let from = at(2026, 10, 20, 8, 0);
        let to = from + 4 * 86_400;
        let events = parse_ics(FEED, 1, from, to, offset);
        let names: Vec<(String, i64)> = events.iter().map(|e| (e.title.clone(), e.start)).collect();
        assert_eq!(names, vec![
            ("Dentista".into(), at(2026, 10, 20, 14, 30)),
            ("Compleanno".into(), at(2026, 10, 20, 22, 0)),
            ("Bolletta".into(), at(2026, 10, 21, 22, 0)),
            // Wednesday 21: EXDATE. Friday 23: moved to 11:00 by its RECURRENCE-ID.
            ("Stand-up (spostato)".into(), at(2026, 10, 23, 9, 0)),
        ]);
        assert!(events[1].all_day && !events[0].all_day);
        assert_eq!(events[0].location.as_deref(), Some("Via Roma 1"));
        assert_eq!(events[0].end - events[0].start, H);
        // The Monday before is out of the window, the Monday after too.
        let wider = parse_ics(FEED, 1, from - 2 * 86_400, to + 3 * 86_400, offset);
        let standups: Vec<i64> = wider.iter().filter(|e| e.uid == "weekly").map(|e| e.start).collect();
        assert_eq!(standups, vec![at(2026, 10, 19, 7, 15), at(2026, 10, 23, 9, 0), at(2026, 10, 26, 7, 15)]);
    }

    #[test]
    fn count_until_interval_and_monthly_weekdays_are_honoured() {
        let offset = 0;
        let start = at(2026, 1, 6, 9, 0); // a Tuesday
        let far = at(2030, 1, 1, 0, 0);
        let rule = |s: &str| parse_rule(s, offset).unwrap();
        assert_eq!(expand(start, &rule("FREQ=DAILY;COUNT=3"), far, offset), vec![start, start + 86_400, start + 2 * 86_400]);
        assert_eq!(expand(start, &rule("FREQ=DAILY;INTERVAL=7;UNTIL=20260120T090000Z"), far, offset), vec![start, start + 7 * 86_400, start + 14 * 86_400]);
        assert_eq!(expand(start, &rule("FREQ=WEEKLY;INTERVAL=2;BYDAY=TU;COUNT=2"), far, offset), vec![start, start + 14 * 86_400]);
        // Second Tuesday of each month, from January 2026.
        let second = expand(start, &rule("FREQ=MONTHLY;BYDAY=2TU;COUNT=2"), far, offset);
        assert_eq!(second, vec![at(2026, 1, 13, 9, 0), at(2026, 2, 10, 9, 0)]);
        // The 31st only in months that have one.
        let thirty_first = expand(at(2026, 1, 31, 9, 0), &rule("FREQ=MONTHLY;COUNT=3"), far, offset);
        assert_eq!(thirty_first, vec![at(2026, 1, 31, 9, 0), at(2026, 3, 31, 9, 0), at(2026, 5, 31, 9, 0)]);
    }

    #[test]
    fn the_keys_and_the_links() {
        assert_eq!(key(1), "calendar-url");
        assert_eq!(key(3), "calendar-3-url");
        assert_eq!(known_keys().len(), SLOTS);
    }
}
