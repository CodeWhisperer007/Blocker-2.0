//! Development-only HTTP bridge to the command core.
//!
//! Lets the React frontend run in an ordinary browser at `localhost:1420` and
//! drive **real** Rust command logic, rather than mocks that quietly drift from
//! the backend. Same `execute()` the Tauri app and CLI call — this is just a
//! fourth thin shim over it.
//!
//! Why this matters: without it, exercising the UI means launching the desktop
//! app and clicking by hand. With it, the UI is reachable by browser automation,
//! so nearly all behaviour can be tested without a human.
//!
//! ```text
//!   browser (localhost:1420)  ──HTTP──>  devserver (17549)  ──>  execute()
//!   extension                 ──HTTP──>  devserver (17549)  ──>  execute()
//!   Tauri webview             ──IPC───>  #[tauri::command]   ──>  execute()
//!   terminal                  ──────────>  clap subcommand    ──>  execute()
//! ```
//!
//! Not shipped: `publish = false`, and nothing in the release bundle depends on
//! it. It binds loopback only.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use focuser_app::{AppContext, Command, execute};
use focuser_core::{BlockEngine, Database};
use tracing::{Level, error, info, warn};

mod api;

/// The extension's fixed endpoint (see `extension/lib/api.ts`). The devserver
/// now serves the extension API as well as the web frontend, so this is the
/// port both clients expect. If the desktop app is running it already holds
/// this port; stop it first.
const DEFAULT_PORT: u16 = 17549;

/// The Vite dev server. Only this origin is allowed through CORS.
const DEV_ORIGIN: &str = "http://localhost:1420";

/// Vite is reachable under either loopback name; both are still the dev UI.
const DEV_ORIGIN_LOOPBACK: &str = "http://127.0.0.1:1420";

#[derive(Parser)]
#[command(
    name = "focuser-devserver",
    about = "Dev-only HTTP bridge to the Focuser command core"
)]
struct Cli {
    /// Port to listen on (loopback only).
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// SQLite file to use. Defaults to a throwaway dev database so testing
    /// never touches real block lists.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Use an in-memory database — state resets each run, for repeatable tests.
    #[arg(long)]
    memory: bool,

    /// Fill the database with sample block lists and statistics, so the charts
    /// have something to draw without waiting for real usage.
    #[arg(long)]
    seed: bool,
}

fn main() {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();
    let cli = Cli::parse();

    let db = if cli.memory {
        info!("using in-memory database (state resets on exit)");
        Database::open_in_memory().expect("failed to open in-memory database")
    } else {
        let path = cli.db.unwrap_or_else(|| PathBuf::from("focuser-dev.db"));
        info!(path = %path.display(), "using dev database");
        Database::open(&path).expect("failed to open dev database")
    };

    if cli.seed {
        seed(&db).expect("failed to seed sample data");
        info!("seeded sample block lists and statistics");
    }

    let engine = BlockEngine::new(db).expect("failed to build engine");
    // Headless: no hosts-file writes. A dev server must never edit the real
    // system hosts file as a side effect of a UI click. It does still report
    // connected extensions so website allowances can count down.
    let ctx = Arc::new(AppContext::new(engine, Arc::new(api::DevSync)));

    let addr = format!("127.0.0.1:{}", cli.port);
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        panic!("cannot bind {addr}: {e}");
    });

    info!(
        %addr,
        origin = DEV_ORIGIN,
        "devserver ready — POST /command (frontend) and /api/* (extension)"
    );

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let ctx = Arc::clone(&ctx);
                std::thread::spawn(move || {
                    if let Err(e) = handle(stream, &ctx) {
                        warn!(error = %e, "request failed");
                    }
                });
            }
            Err(e) => error!(error = %e, "accept failed"),
        }
    }
}

/// Sample sites, apps and 30 days of statistics.
///
/// The shapes are deliberately different per site — a steady one, a weekend
/// spike, a decline — because a chart drawn from flat data looks fine no matter
/// how wrong it is.
fn seed(db: &Database) -> focuser_common::Result<()> {
    use focuser_common::types::{AppRule, BlockList, WebsiteRule};

    let mut work = BlockList::new("Deep work");
    for domain in ["reddit.com", "youtube.com", "x.com", "news.ycombinator.com"] {
        work.websites.push(WebsiteRule::domain(domain));
    }
    work.applications.push(AppRule::executable("steam.exe"));
    db.create_block_list(&work)?;

    let mut evenings = BlockList::new("Evenings");
    for domain in ["netflix.com", "twitch.tv"] {
        evenings.websites.push(WebsiteRule::domain(domain));
    }
    evenings.enabled = false;
    db.create_block_list(&evenings)?;

    let sites: [(&str, f64, f64); 9] = [
        ("reddit.com", 14.0, 0.0),
        ("youtube.com", 11.0, 1.1),
        ("x.com", 9.0, 2.2),
        ("news.ycombinator.com", 7.0, 3.3),
        ("instagram.com", 6.0, 0.7),
        ("twitch.tv", 5.0, 4.0),
        ("tiktok.com", 4.0, 1.8),
        ("netflix.com", 3.0, 2.6),
        ("steam.exe", 3.0, 5.1),
    ];

    let today = chrono::Local::now().date_naive();
    for back in 0..30 {
        let date = (today - chrono::Duration::days(back)).to_string();
        for (name, scale, phase) in sites {
            let wave = (f64::from(back as i32).mul_add(0.45, phase))
                .sin()
                .mul_add(0.5, 0.75);
            let attempts = (scale * wave).round() as i64;
            if attempts > 0 {
                db.record_usage_on(name, &date, attempts, attempts * 47)?;
            }
        }
    }

    Ok(())
}

fn handle(mut stream: TcpStream, ctx: &AppContext) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    // Headers we care about. Header names are case-insensitive per RFC 9110.
    let mut content_length = 0usize;
    let mut content_type: Option<String> = None;
    let mut origin: Option<String> = None;
    let mut focuser_browser: Option<String> = None;
    let mut if_none_match: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.parse().unwrap_or(0),
            "content-type" => content_type = Some(value.to_string()),
            "origin" => origin = Some(value.to_string()),
            "x-focuser-browser" => focuser_browser = Some(value.to_string()),
            "if-none-match" => if_none_match = Some(value.to_string()),
            _ => {}
        }
    }

    // Read the body once, before routing: both `/command` and some `/api/*`
    // endpoints take one.
    let mut body = String::new();
    if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        reader.read_exact(&mut buf)?;
        body = String::from_utf8_lossy(&buf).to_string();
    }

    // Every mutating endpoint expects JSON. Requiring it is a CSRF guard: a
    // `text/plain` POST is a "simple" request that skips CORS preflight, so
    // without this any web page could drive the local API blind. `fetch` with
    // `Content-Type: application/json` is not simple, so its preflight hits the
    // origin check below and a foreign origin is refused.
    if method == "POST" && !is_json(content_type.as_deref()) {
        return respond(
            &mut stream,
            415,
            r#"{"error":"Content-Type must be application/json"}"#,
            Some("application/json"),
            &[],
            allowed_origin(
                origin.as_deref(),
                path == "/api" || path.starts_with("/api/"),
            ),
        );
    }

    // The extension's half of the surface lives on the same port as the web
    // frontend's. Everything under `/api/` goes to the ported desktop handlers.
    if path == "/api" || path.starts_with("/api/") {
        let response = api::handle(
            &method,
            &path,
            &body,
            focuser_browser.as_deref(),
            if_none_match.as_deref(),
            ctx,
        );
        let allow = allowed_origin(origin.as_deref(), true);
        return respond(
            &mut stream,
            response.status,
            &response.body,
            Some("application/json"),
            &response.headers,
            allow,
        );
    }

    match (method.as_str(), path.as_str()) {
        // Preflight for the Vite origin.
        ("OPTIONS", _) => respond(
            &mut stream,
            204,
            "",
            None,
            &[],
            allowed_origin(origin.as_deref(), false),
        ),

        ("GET", "/health") => respond(
            &mut stream,
            200,
            &serde_json::json!({ "ok": true }).to_string(),
            Some("application/json"),
            &[],
            allowed_origin(origin.as_deref(), false),
        ),

        ("POST", "/command") => {
            let (status, payload) = run_command(ctx, body.as_bytes());
            respond(
                &mut stream,
                status,
                &payload,
                Some("application/json"),
                &[],
                allowed_origin(origin.as_deref(), false),
            )
        }

        _ => respond(
            &mut stream,
            404,
            &serde_json::json!({ "error": "not found" }).to_string(),
            Some("application/json"),
            &[],
            allowed_origin(origin.as_deref(), false),
        ),
    }
}

/// Which `Origin` may read the response.
///
/// `/command` and `/health` are for the Vite dev UI. `/api/*` is for the
/// extension, whose origin is a per-install `chrome-extension://` /
/// `moz-extension://` id — much safer than the desktop app's
/// `Access-Control-Allow-Origin: *`, which let any web page mutate block lists.
/// A request with no origin needs no CORS header at all (and the extension's
/// `<all_urls>` host permission bypasses CORS anyway).
fn allowed_origin(origin: Option<&str>, extension_api: bool) -> Option<&str> {
    let origin = origin?;
    if origin == DEV_ORIGIN || origin == DEV_ORIGIN_LOOPBACK {
        return Some(origin);
    }
    if extension_api
        && (origin.starts_with("chrome-extension://") || origin.starts_with("moz-extension://"))
    {
        return Some(origin);
    }
    None
}

/// Whether a request body claims to be JSON.
///
/// Parameters such as `; charset=utf-8` are ignored; only the media type
/// matters for the CSRF guard above.
fn is_json(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

/// Deserialise a [`Command`], run it, and render the outcome as JSON.
///
/// Errors come back as `{ "code": ..., "message": ... }` with a non-2xx status,
/// so the frontend can branch on `code` without parsing prose.
fn run_command(ctx: &AppContext, body: &[u8]) -> (u16, String) {
    let cmd: Command = match serde_json::from_slice(body) {
        Ok(c) => c,
        Err(e) => {
            return (
                400,
                serde_json::json!({ "code": "bad_request", "message": e.to_string() }).to_string(),
            );
        }
    };

    // The engine caches block lists in memory. In the desktop app the blocking
    // loop refreshes that every few seconds; here nothing does, so a change made
    // by the CLI against the same file would stay invisible. Refresh per request
    // — it keeps the CLI usable as a test harness alongside the browser.
    if let Ok(mut engine) = ctx.engine.lock() {
        let _ = engine.refresh();
    }

    match execute(ctx, cmd) {
        Ok(result) => (
            200,
            serde_json::to_string(&result).unwrap_or_else(|e| {
                error!(error = %e, "result failed to serialise");
                r#"{"code":"internal","message":"result serialisation failed"}"#.to_string()
            }),
        ),
        Err(err) => {
            // 4xx for caller mistakes, 500 for genuine backend faults, so
            // browser devtools shows the right colour at a glance.
            let status = match err.exit_code() {
                1 => 500,
                _ => 400,
            };
            (
                status,
                serde_json::to_string(&err)
                    .unwrap_or_else(|_| r#"{"code":"internal","message":"unknown"}"#.to_string()),
            )
        }
    }
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
    content_type: Option<&str>,
    extra_headers: &[(String, String)],
    allow_origin: Option<&str>,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        415 => "Unsupported Media Type",
        _ => "Internal Server Error",
    };

    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    if let Some(origin) = allow_origin {
        head.push_str(&format!("Access-Control-Allow-Origin: {origin}\r\n"));
        head.push_str("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n");
        head.push_str("Access-Control-Allow-Headers: Content-Type, X-Focuser-Browser\r\n");
        head.push_str("Vary: Origin\r\n");
    }

    // A 304 has no body, and must not claim a length the full answer lacks.
    let len = if status == 304 { 0 } else { body.len() };
    head.push_str(&format!("Content-Length: {len}\r\n"));
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");

    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}
