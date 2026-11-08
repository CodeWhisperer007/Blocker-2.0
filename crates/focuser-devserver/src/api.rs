//! Extension-facing local HTTP API for the devserver.
//!
//! The browser extension talks to `http://127.0.0.1:17549/api/*` (see
//! `extension/lib/api.ts`), while the web frontend uses `POST /command` and
//! `GET /health`. This module adds the extension half to the devserver so a
//! single process on one port can serve both clients.
//!
//! Ported from the desktop app's `focuser-ui::api`, which is authoritative for
//! the wire format. Keeping the behaviour identical means the extension does
//! not need a dev-only branch.
//!
//! Security notes:
//! - The listener is bound to loopback by `main`.
//! - Responses echo only expected origins instead of `Access-Control-Allow-
//!   Origin: *`, so an arbitrary web page cannot read or mutate block lists.
//!   The extension itself is allowed through its `chrome-extension://` /
//!   `moz-extension://` origin (and works even without CORS thanks to its
//!   `<all_urls>` host permission).
//! - The devserver is headless: `DevSync` deliberately never touches the real
//!   hosts file.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use focuser_app::{AppContext, SystemSync};
use focuser_common::extension::BrowserType;
use focuser_common::host::{any_host_matches, canonical_host};
use focuser_common::types::WebsiteMatchType;
use tracing::{debug, warn};

/// How recently an extension must have checked in to count as connected.
pub const EXTENSION_SEEN_SECS: u64 = 120;

/// A browser's extension last said something, and whether it covers private
/// windows.
#[derive(Debug, Clone)]
struct BrowserHeartbeat {
    last_seen: Instant,
    /// One entry per browser profile. Empty until an extension reports, so an
    /// extension too old to report keeps the benefit of the doubt.
    private_windows: HashMap<String, (Instant, bool)>,
}

/// A browser runs a handful of profiles. More ids than this is something else
/// talking to the port, and it does not get to grow the map without end.
const MAX_PROFILES: usize = 16;

impl BrowserHeartbeat {
    fn new() -> Self {
        Self {
            last_seen: Instant::now(),
            private_windows: HashMap::new(),
        }
    }

    /// Note a check-in. `None` is a rules poll, which says nothing about
    /// private windows and keeps the last real answers.
    fn record(&mut self, report: Option<(&str, bool)>) {
        let now = Instant::now();
        self.last_seen = now;
        let Some((profile, allowed)) = report else {
            return;
        };
        if !self.private_windows.contains_key(profile) && self.private_windows.len() >= MAX_PROFILES
        {
            let stalest = self
                .private_windows
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(id, _)| id.clone());
            if let Some(id) = stalest {
                self.private_windows.remove(&id);
            }
        }
        self.private_windows
            .insert(profile.to_string(), (now, allowed));
    }

    /// Only a profile that said, within `within`, that it cannot see private
    /// windows leaves a gap. One that has gone quiet drops out.
    fn covers_private_windows(&self, within: std::time::Duration) -> bool {
        !self
            .private_windows
            .values()
            .any(|(at, allowed)| !allowed && at.elapsed() < within)
    }
}

/// `true` or `false` and nothing else. Unknown is not "no": a stray value must
/// not get a browser closed.
fn parse_flag(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Tracks last-seen time and incognito coverage for each browser's extension.
static CONNECTED_BROWSERS: Mutex<Option<HashMap<BrowserType, BrowserHeartbeat>>> = Mutex::new(None);

/// Server-side dedup window for blocked reports, so multi-tab navigation and
/// service-worker restarts do not inflate the count.
static RECENT_REPORTS: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
const REPORT_DEDUP_WINDOW_SECS: u64 = 3;

fn should_record_blocked(tracking_key: &str) -> bool {
    let now = Instant::now();
    let mut guard = RECENT_REPORTS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);

    let window = std::time::Duration::from_secs(REPORT_DEDUP_WINDOW_SECS * 4);
    map.retain(|_, ts| now.duration_since(*ts) < window);

    if let Some(last) = map.get(tracking_key)
        && now.duration_since(*last) < std::time::Duration::from_secs(REPORT_DEDUP_WINDOW_SECS)
    {
        return false;
    }
    map.insert(tracking_key.to_string(), now);
    true
}

fn record_extension_heartbeat(browser_name: &str, private_windows: Option<(&str, bool)>) {
    let Some(browser_type) = browser_type_named(browser_name) else {
        return;
    };

    let mut guard = CONNECTED_BROWSERS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(browser_type)
        .or_insert_with(BrowserHeartbeat::new)
        .record(private_windows);
    debug!(browser = browser_name, "extension heartbeat recorded");
}

/// The browser an extension says it runs in.
fn browser_type_named(name: &str) -> Option<BrowserType> {
    match name.to_lowercase().as_str() {
        "chrome" => Some(BrowserType::Chrome),
        "firefox" => Some(BrowserType::Firefox),
        "edge" => Some(BrowserType::Edge),
        "brave" => Some(BrowserType::Brave),
        "opera" => Some(BrowserType::Opera),
        _ => None,
    }
}

fn connected_browsers_matching(
    timeout_secs: u64,
    predicate: impl Fn(&BrowserHeartbeat, std::time::Duration) -> bool,
) -> std::collections::HashSet<BrowserType> {
    let cutoff = std::time::Duration::from_secs(timeout_secs);
    let now = Instant::now();
    let guard = CONNECTED_BROWSERS.lock().unwrap();

    match guard.as_ref() {
        Some(map) => map
            .iter()
            .filter(|(_, hb)| now.duration_since(hb.last_seen) < cutoff && predicate(hb, cutoff))
            .map(|(bt, _)| bt.clone())
            .collect(),
        None => std::collections::HashSet::new(),
    }
}

/// The browser opened the address the extension left for its removal.
///
/// Only an id the extension sent before counts: any web page can open this
/// address, and none knows that id.
fn forget_removed_extension(browser_name: &str, profile: &str) {
    let Some(browser_type) = browser_type_named(browser_name) else {
        return;
    };
    let mut guard = CONNECTED_BROWSERS.lock().unwrap();
    let Some(map) = guard.as_mut() else {
        return;
    };
    let known = map
        .get(&browser_type)
        .is_some_and(|hb| !profile.is_empty() && hb.private_windows.contains_key(profile));
    if known {
        map.remove(&browser_type);
        tracing::info!(browser = browser_name, "extension removed from the browser");
    }
}

/// The system side effect the command core expects.
///
/// The devserver must not edit the real hosts file, but it does report which
/// extensions checked in — without that, `allowance_exempt_domains` would
/// always answer "no extension", and website allowances would never start.
pub struct DevSync;

impl SystemSync for DevSync {
    fn sync_hosts(&self, _domains: &[String]) {}

    fn connected_browsers(&self) -> Vec<String> {
        connected_browsers_matching(EXTENSION_SEEN_SECS, |_, _| true)
            .iter()
            .map(|b| format!("{b:?}"))
            .collect()
    }

    fn safely_connected_browsers(&self) -> Vec<String> {
        connected_browsers_matching(
            EXTENSION_SEEN_SECS,
            BrowserHeartbeat::covers_private_windows,
        )
        .iter()
        .map(|b| format!("{b:?}"))
        .collect()
    }
}

/// One response from the extension API.
pub struct ApiResponse {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

impl ApiResponse {
    fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            headers: Vec::new(),
        }
    }
}

/// Route a request whose path starts with `/api/`.
pub fn handle(
    method: &str,
    path: &str,
    body: &str,
    focuser_browser: Option<&str>,
    if_none_match: Option<&str>,
    state: &AppContext,
) -> ApiResponse {
    // The `X-Focuser-Browser` header rides every `/api/rules` poll. It carries
    // no incognito information, so only recency is updated here.
    if let Some(browser_name) = focuser_browser {
        record_extension_heartbeat(browser_name, None);
    }

    // Handle CORS preflight before routing: an OPTIONS request carries no
    // body and no useful path match.
    if method == "OPTIONS" {
        return ApiResponse::new(204, "");
    }

    let (status, body) = route(method, path, body, state);
    let mut response = ApiResponse::new(status, body);

    if status == 302 {
        // A redirect carries where it goes in place of a body.
        response
            .headers
            .push(("Location".into(), response.body.clone()));
        response.body.clear();
    } else if method == "GET" && path == "/api/rules" && status == 200 {
        // The extension asks every two seconds. An unchanged list costs a few
        // bytes instead of the whole list each time.
        let tag = content_tag(&response.body);
        response.headers.push(("ETag".into(), tag.clone()));
        response
            .headers
            .push(("Cache-Control".into(), "no-cache".into()));
        if if_none_match == Some(tag.as_str()) {
            response.status = 304;
            response.body.clear();
        }
    }

    response
}

/// A tag for a response body. The same body always gets the same tag.
fn content_tag(body: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    format!("\"{:016x}\"", hasher.finish())
}

fn route(method: &str, path: &str, body: &str, state: &AppContext) -> (u16, String) {
    match (method, path) {
        ("GET", "/api/status") => api_status(state),
        ("GET", "/api/lists") => api_lists(state),
        ("GET", "/api/rules") => api_rules(state),
        ("POST", "/api/add-site") => api_add_site(body, state),
        ("POST", "/api/remove-site") => api_remove_site(body, state),
        ("POST", "/api/toggle-list") => api_toggle_list(body, state),
        ("POST", "/api/blocked") => api_report_blocked(body, state),
        ("POST", "/api/allowance-tick") => api_allowance_tick(body, state),
        ("GET", "/api/allowance-blocked") => api_allowance_blocked(state),
        ("GET", "/api/pomodoro") => api_pomodoro_status(state),
        ("GET", "/api/allowances") => api_allowances_list(state),
        // The devserver has no window to raise; answering keeps the popup
        // working the same way it does against the desktop app.
        ("POST", "/api/show") | ("GET", "/api/show") => (200, r#"{"ok":true}"#.into()),
        _ if path.starts_with("/api/site-status?") => {
            api_site_status(path.split_once('?').map(|(_, q)| q).unwrap_or(""), state)
        }
        _ if path.starts_with("/api/check?") => api_check_domain(path, state),
        _ if path.starts_with("/api/blocked-count?") => api_blocked_count(path, state),
        _ if path.starts_with("/api/heartbeat?") => {
            let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
            let browser = query_param(query, "browser").unwrap_or("");
            let allowed = query_param(query, "incognito_allowed").and_then(parse_flag);
            let profile = query_param(query, "profile").unwrap_or("");
            if !browser.is_empty() {
                record_extension_heartbeat(browser, allowed.map(|allowed| (profile, allowed)));
            }
            (200, r#"{"ok":true}"#.into())
        }
        _ if path.starts_with("/api/extension-removed?") => {
            let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
            let browser = query_param(query, "browser").unwrap_or("");
            forget_removed_extension(browser, query_param(query, "profile").unwrap_or(""));
            let store = match browser_type_named(browser) {
                Some(BrowserType::Firefox) => focuser_common::browser::FIREFOX_STORE_URL,
                _ => focuser_common::browser::CHROME_STORE_URL,
            };
            (302, store.into())
        }
        _ => (404, r#"{"error":"not found"}"#.into()),
    }
}

fn api_status(state: &AppContext) -> (u16, String) {
    let eng = state.engine.lock().unwrap();
    let lists = eng.block_lists();
    let active_count = lists.iter().filter(|l| l.enabled).count();
    let total_sites: usize = lists
        .iter()
        .filter(|l| l.enabled)
        .map(|l| l.websites.len())
        .sum();
    let total_apps: usize = lists
        .iter()
        .filter(|l| l.enabled)
        .map(|l| l.applications.len())
        .sum();
    let blocked_today = eng.db().get_total_blocked_today().unwrap_or(0);

    let json = serde_json::json!({
        "running": true,
        "active_lists": active_count,
        "blocked_sites": total_sites,
        "blocked_apps": total_apps,
        "blocked_today": blocked_today,
    });
    (200, json.to_string())
}

fn api_lists(state: &AppContext) -> (u16, String) {
    let eng = state.engine.lock().unwrap();
    let lists: Vec<serde_json::Value> = eng
        .block_lists()
        .iter()
        .map(|l| {
            serde_json::json!({
                "id": l.id.to_string(),
                "name": l.name,
                "enabled": l.enabled,
                "website_count": l.websites.len(),
                "app_count": l.applications.len(),
            })
        })
        .collect();
    (200, serde_json::to_string(&lists).unwrap_or_default())
}

fn api_rules(state: &AppContext) -> (u16, String) {
    let eng = state.engine.lock().unwrap();
    let allowance_exceptions = state.allowance_exempt_domains(&eng);
    let mut rules = eng.compile_extension_rules_with_allowances(
        &allowance_exceptions,
        &spent_domains(state, &eng),
    );
    // `locked` only tells the extension to keep the extensions page shut, so
    // with that turned off it is never sent.
    rules.locked &= guards_extensions_page(eng.db());

    let mut domain_categories: HashMap<String, String> = HashMap::new();
    for list in eng.block_lists().iter().filter(|l| l.enabled) {
        let category = normalize_category(&list.name);
        for rule in &list.websites {
            if let WebsiteMatchType::Domain(ref d) = rule.match_type {
                domain_categories.insert(canonical_host(d), category.clone());
            }
        }
    }

    let mut response = serde_json::to_value(&rules).unwrap_or_default();
    if let Some(obj) = response.as_object_mut() {
        obj.insert(
            "domain_categories".to_string(),
            serde_json::to_value(&domain_categories).unwrap_or_default(),
        );
    }

    (200, response.to_string())
}

fn guards_extensions_page(db: &focuser_core::db::Database) -> bool {
    db.get_setting_or_default(focuser_app::execute::SETTING_GUARD_EXTENSIONS_PAGE, "true")
        .map_or(true, |v| v != "false")
}

fn normalize_category(list_name: &str) -> String {
    let name = list_name.to_lowercase();
    for (keyword, category) in [
        ("porn", "adult"),
        ("adult", "adult"),
        ("nsfw", "adult"),
        ("xxx", "adult"),
        ("social", "social_media"),
        ("instagram", "social_media"),
        ("facebook", "social_media"),
        ("twitter", "social_media"),
        ("tiktok", "social_media"),
        ("video", "video"),
        ("stream", "video"),
        ("youtube", "video"),
        ("netflix", "video"),
        ("entertain", "video"),
        ("dating", "dating"),
        ("tinder", "dating"),
        ("bumble", "dating"),
        ("gaming", "gaming"),
        ("game", "gaming"),
        ("steam", "gaming"),
        ("news", "news"),
        ("reddit", "news"),
        ("forum", "news"),
        ("shop", "shopping"),
        ("amazon", "shopping"),
        ("ebay", "shopping"),
        ("gambling", "gambling"),
        ("betting", "gambling"),
        ("casino", "gambling"),
    ] {
        if name.contains(keyword) {
            return category.to_string();
        }
    }
    "default".to_string()
}

fn api_check_domain(path: &str, state: &AppContext) -> (u16, String) {
    let domain = path
        .split("domain=")
        .nth(1)
        .unwrap_or("")
        .split('&')
        .next()
        .unwrap_or("");

    let eng = state.engine.lock().unwrap();
    let exemptions = state.allowance_exempt_domains(&eng);
    let blocked = if any_host_matches(&exemptions, domain) && !eng.scheduled_block_on_domain(domain)
    {
        false
    } else {
        eng.check_domain(domain).is_some() || any_host_matches(spent_domains(state, &eng), domain)
    };
    let json = serde_json::json!({ "domain": domain, "blocked": blocked });
    (200, json.to_string())
}

fn api_add_site(body: &str, state: &AppContext) -> (u16, String) {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return (400, format!(r#"{{"error":"{}"}}"#, e)),
    };

    let list_id = parsed["list_id"].as_str().unwrap_or("");
    let domain = parsed["domain"].as_str().unwrap_or("");
    let rule_type = parsed["rule_type"].as_str().unwrap_or("domain");

    if list_id.is_empty() || domain.is_empty() {
        return (400, r#"{"error":"list_id and domain required"}"#.into());
    }

    let uuid = match uuid::Uuid::parse_str(list_id) {
        Ok(u) => u,
        Err(e) => return (400, format!(r#"{{"error":"{}"}}"#, e)),
    };

    let mut eng = state.engine.lock().unwrap();
    // No lock check: a new site only blocks more, so a locked list takes one.
    let mut list = match eng.db().get_block_list(uuid) {
        Ok(l) => l,
        Err(e) => return (404, format!(r#"{{"error":"{}"}}"#, e)),
    };

    use focuser_common::types::WebsiteRule;
    let rule = match rule_type {
        "keyword" => WebsiteRule::keyword(domain),
        "wildcard" => WebsiteRule::wildcard(domain),
        "url_path" => WebsiteRule::url_path(domain),
        _ => WebsiteRule::domain(domain),
    };

    // Adding the same site twice used to create a second identical rule, which
    // is invisible in the popup and has to be deleted twice in the app.
    let already = list
        .websites
        .iter()
        .any(|existing| existing.match_type == rule.match_type);
    if already {
        return (
            200,
            serde_json::json!({ "ok": true, "added": 0, "duplicate": true }).to_string(),
        );
    }

    list.websites.push(rule);
    list.updated_at = chrono::Utc::now();

    if let Err(e) = eng.db().update_block_list(&list) {
        return (500, format!(r#"{{"error":"{}"}}"#, e));
    }
    let _ = eng.refresh();
    state.sync_hosts(&eng);

    (
        200,
        serde_json::json!({ "ok": true, "added": 1, "duplicate": false }).to_string(),
    )
}

fn api_remove_site(body: &str, state: &AppContext) -> (u16, String) {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return (400, format!(r#"{{"error":"{}"}}"#, e)),
    };

    let list_id = parsed["list_id"].as_str().unwrap_or("");
    let rule_id = parsed["rule_id"].as_str().unwrap_or("");
    let domain = parsed["domain"].as_str().unwrap_or("");

    // Support removal by rule_id OR by domain name
    if list_id.is_empty() || (rule_id.is_empty() && domain.is_empty()) {
        return (
            400,
            r#"{"error":"list_id and (rule_id or domain) required"}"#.into(),
        );
    }

    let uuid = match uuid::Uuid::parse_str(list_id) {
        Ok(u) => u,
        Err(e) => return (400, format!(r#"{{"error":"{}"}}"#, e)),
    };

    let mut eng = state.engine.lock().unwrap();
    let mut list = match eng.db().get_block_list(uuid) {
        Ok(l) => l,
        Err(e) => return (404, format!(r#"{{"error":"{}"}}"#, e)),
    };
    if list.is_modification_protected() || eng.is_block_list_protected(uuid) {
        return (403, serde_json::json!({"error": "protected"}).to_string());
    }

    let before = list.websites.len();
    let mut left_behind = 0usize;

    if !rule_id.is_empty() {
        list.websites.retain(|r| r.id.to_string() != rule_id);
    } else if !domain.is_empty() {
        let domain_lower = domain.to_lowercase();
        list.websites.retain(|rule| {
            if !rule.matches_domain(&domain_lower) {
                return true;
            }
            // Only rules that are *about* this host go. A keyword like "tube"
            // matches youtube.com but belongs to the user's wider intent, so it
            // is reported back instead of being deleted behind their back.
            match rule.match_type {
                WebsiteMatchType::Domain(_) | WebsiteMatchType::Wildcard(_) => false,
                _ => {
                    left_behind += 1;
                    true
                }
            }
        });
    }

    let removed = before - list.websites.len();
    list.updated_at = chrono::Utc::now();

    if let Err(e) = eng.db().update_block_list(&list) {
        return (500, format!(r#"{{"error":"{}"}}"#, e));
    }
    let _ = eng.refresh();
    state.sync_hosts(&eng);

    // `removed` matters: removing nothing used to answer "ok" exactly like a
    // real removal, so the popup reported success while the site stayed blocked.
    (
        200,
        serde_json::json!({ "ok": true, "removed": removed, "left_behind": left_behind })
            .to_string(),
    )
}

/// Which lists hold a rule for this host, so a caller can say where a site is
/// blocked rather than making the user guess.
fn api_site_status(query: &str, state: &AppContext) -> (u16, String) {
    let domain = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("domain="))
        .map(|raw| percent_decode(raw).to_lowercase())
        .unwrap_or_default();

    if domain.is_empty() {
        return (400, r#"{"error":"domain required"}"#.into());
    }

    let eng = state.engine.lock().unwrap();
    let lists: Vec<serde_json::Value> = eng
        .block_lists()
        .iter()
        .filter_map(|list| {
            // Rule presence, not `should_block_domain`: a disabled list still
            // holds the site, and that is what the caller needs to act on.
            let matched = list
                .websites
                .iter()
                .find(|rule| rule.enabled && rule.matches_domain(&domain))?;
            Some(serde_json::json!({
                "id": list.id.to_string(),
                "name": list.name,
                "enabled": list.enabled,
                "rule_kind": match matched.match_type {
                    WebsiteMatchType::Domain(_) => "domain",
                    WebsiteMatchType::Wildcard(_) => "wildcard",
                    WebsiteMatchType::Keyword(_) => "keyword",
                    WebsiteMatchType::UrlPath(_) => "url_path",
                    WebsiteMatchType::EntireInternet => "everything",
                },
            }))
        })
        .collect();

    let blocked = eng.check_domain(&domain).is_some();
    (
        200,
        serde_json::json!({ "domain": domain, "blocked": blocked, "lists": lists }).to_string(),
    )
}

fn api_report_blocked(body: &str, state: &AppContext) -> (u16, String) {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return (400, format!(r#"{{"error":"{}"}}"#, e)),
    };

    let domain = parsed["domain"].as_str().unwrap_or("");
    if domain.is_empty() {
        return (400, r#"{"error":"domain required"}"#.into());
    }

    let tracking_key = parsed["tracking_key"].as_str().unwrap_or(domain);

    // Server-side dedup: only record if this exact tracking_key wasn't recorded
    // within the last REPORT_DEDUP_WINDOW_SECS seconds.
    let recorded = should_record_blocked(tracking_key);

    let eng = state.engine.lock().unwrap();

    if recorded {
        let _ = eng.record_blocked(domain);
        if tracking_key != domain {
            let _ = eng.db().record_blocked_attempt(tracking_key);
        }
    }

    // Always return the current count (whether or not we just incremented).
    let count = if tracking_key != domain {
        eng.db().get_blocked_count_today(tracking_key).unwrap_or(0)
    } else {
        eng.db().get_blocked_count_today(domain).unwrap_or(0)
    };

    let json = serde_json::json!({ "ok": true, "count": count, "recorded": recorded });
    (200, json.to_string())
}

fn api_blocked_count(path: &str, state: &AppContext) -> (u16, String) {
    let key = path
        .split("key=")
        .nth(1)
        .unwrap_or("")
        .split('&')
        .next()
        .unwrap_or("");
    let decoded = percent_decode(key);
    let eng = state.engine.lock().unwrap();
    let count = eng.db().get_blocked_count_today(&decoded).unwrap_or(0);
    let json = serde_json::json!({ "key": decoded, "count": count });
    (200, json.to_string())
}

/// Pull one `key=value` pair out of a query string, unescaped only in the
/// sense of stopping at `&`.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix(key).and_then(|v| v.strip_prefix('=')) {
            return Some(value);
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(h) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(h, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn api_toggle_list(body: &str, state: &AppContext) -> (u16, String) {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return (400, format!(r#"{{"error":"{}"}}"#, e)),
    };

    let list_id = parsed["list_id"].as_str().unwrap_or("");
    let enabled = parsed["enabled"].as_bool().unwrap_or(true);

    let uuid = match uuid::Uuid::parse_str(list_id) {
        Ok(u) => u,
        Err(e) => return (400, format!(r#"{{"error":"{}"}}"#, e)),
    };

    let mut eng = state.engine.lock().unwrap();
    let mut list = match eng.db().get_block_list(uuid) {
        Ok(l) => l,
        Err(e) => return (404, format!(r#"{{"error":"{}"}}"#, e)),
    };
    if list.is_modification_protected() || eng.is_block_list_protected(uuid) {
        return (403, serde_json::json!({"error": "protected"}).to_string());
    }

    list.enabled = enabled;
    list.updated_at = chrono::Utc::now();

    if let Err(e) = eng.db().update_block_list(&list) {
        return (500, format!(r#"{{"error":"{}"}}"#, e));
    }
    let _ = eng.refresh();
    state.sync_hosts(&eng);

    (200, r#"{"ok":true}"#.into())
}

fn api_allowance_tick(body: &str, state: &AppContext) -> (u16, String) {
    let tick: focuser_common::allowance::AllowanceTick = match serde_json::from_str(body) {
        Ok(t) => t,
        Err(e) => {
            debug!(error = %e, body = %body, "bad allowance-tick payload");
            return (400, format!(r#"{{"error":"{e}"}}"#));
        }
    };
    let eng = match state.engine.lock() {
        Ok(e) => e,
        Err(_) => return (500, r#"{"error":"lock"}"#.into()),
    };
    if let Err(e) = state.allowance_tracker.ingest_tick(eng.db(), &tick) {
        warn!(error = %e, "allowance-tick ingest failed");
        return (500, format!(r#"{{"error":"{e}"}}"#));
    }
    debug!(
        host = ?tick.hostname,
        app = ?tick.app_exe,
        active = tick.active,
        inc = ?tick.increment_secs,
        "allowance tick ingested"
    );
    (200, r#"{"ok":true}"#.into())
}

/// Sites whose allowance is used up for today. A running shared allowance
/// takes over the ones it covers.
fn spent_domains(state: &AppContext, eng: &focuser_core::BlockEngine) -> Vec<String> {
    state
        .allowance_tracker
        .blocked_domains()
        .into_iter()
        .filter(|d| !eng.shared_covers_domain(d))
        .collect()
}

fn api_allowance_blocked(state: &AppContext) -> (u16, String) {
    let eng = state.engine.lock().unwrap();
    let domains = spent_domains(state, &eng);
    let apps: Vec<_> = state
        .allowance_tracker
        .blocked_apps()
        .into_iter()
        .filter(|a| !eng.shared_covers_app(a))
        .collect();
    let json = serde_json::json!({ "domains": domains, "apps": apps });
    (200, json.to_string())
}

fn api_pomodoro_status(state: &AppContext) -> (u16, String) {
    let eng = match state.engine.lock() {
        Ok(e) => e,
        Err(_) => return (500, r#"{"error":"lock"}"#.into()),
    };
    match focuser_core::pomodoro::build_status(eng.db()) {
        Ok(Some(s)) => (
            200,
            serde_json::to_string(&s).unwrap_or_else(|_| "null".into()),
        ),
        Ok(None) => (200, "null".into()),
        Err(e) => (500, format!(r#"{{"error":"{e}"}}"#)),
    }
}

fn api_allowances_list(state: &AppContext) -> (u16, String) {
    let eng = match state.engine.lock() {
        Ok(e) => e,
        Err(_) => return (500, r#"{"error":"lock"}"#.into()),
    };
    match eng.db().allowance_statuses_with_shared() {
        Ok(list) => (
            200,
            serde_json::to_string(&list).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => (500, format!(r#"{{"error":"{e}"}}"#)),
    }
}
