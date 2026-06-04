use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;

use include_dir::{include_dir, Dir};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tauri::{http, ipc::Channel, AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

// The `app/` tree is embedded in the binary at compile time so
// release artifacts ship as a single self-contained file. We
// deliberately do *not* reuse Tauri's asset_resolver (which also
// embeds `app/` via frontendDist) because that resolver
// SPA-fallbacks unknown paths to index.html — disastrous for
// XMLUI's optional code-behind probes that legitimately 404. The
// duplication costs ~6MB; the reliability is worth it.
static EMBEDDED_APP: Dir = include_dir!("$CARGO_MANIFEST_DIR/../app");

// Resolve a path within Bram's own `app/` bundle to (bytes, mime). When
// an on-disk Bram app/ exists, that is ground truth — a missing file is
// genuinely missing. Deliberately do NOT fall back to project-relative
// `cwd/app`: user projects are allowed to have their own app/ folder, and
// letting that shadow Bram's shell assets breaks routing/watchers (#58).
// Only fall back to the embedded tree when there is no on-disk Bram app/.
fn serve_app_file<R: tauri::Runtime>(
    app: Option<&AppHandle<R>>,
    rel: &str,
) -> Option<(Vec<u8>, &'static str)> {
    if let Some(root) = resolve_app_root(app) {
        let p = root.join(rel);
        return std::fs::read(&p).ok().map(|bytes| (bytes, mime_for(&p)));
    }
    EMBEDDED_APP.get_file(rel).map(|file| {
        (
            file.contents().to_vec(),
            mime_for(std::path::Path::new(rel)),
        )
    })
}

// Resolve a path within `app/` to a real on-disk path. If the
// on-disk app_root is present, returns app_root/rel directly. Else
// extracts the embedded file into a per-binary cache dir and returns
// that path. Used for things that need a real filesystem path —
// bash --rcfile, etc. — not just bytes.
fn extract_app_file<R: tauri::Runtime>(app: &AppHandle<R>, rel: &str) -> Result<PathBuf, String> {
    if let Some(root) = resolve_app_root(Some(app)) {
        let p = root.join(rel);
        return if p.exists() {
            Ok(p)
        } else {
            Err(format!("on-disk app file not found: {}", p.display()))
        };
    }
    let file = EMBEDDED_APP
        .get_file(rel)
        .ok_or_else(|| format!("embedded app file not found: {}", rel))?;
    let cache_root = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("no cache dir: {}", e))?
        .join("app");
    let target = cache_root.join(rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&target, file.contents()).map_err(|e| e.to_string())?;
    Ok(target)
}

struct PtyState {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
}

#[derive(Default)]
struct AppState(Mutex<Option<PtyState>>);

// Lifecycle owner for an optional whisper-server child. Spawn via the
// whisper_start command; killed by whisper_stop or on app exit.
#[derive(Default)]
struct WhisperState(Mutex<Option<std::process::Child>>);

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorklistAuthorizationRecord {
    // "approved" | "drop" | "rejected_stale" | "none"
    kind: String,
    #[serde(default)]
    ids: Vec<String>,
    // Full verified item objects, populated when the approve/drop payload
    // arrived with per-item content hashes that matched the on-disk file.
    // Empty for legacy payloads (no hash supplied) and for rejected_stale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    items: Vec<serde_json::Value>,
    // Ids whose supplied hash did not match the current `worklist.json`.
    // Non-empty implies kind == "rejected_stale".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mismatched_ids: Vec<String>,
    issued_at_ms: i64,
    source: String,
    #[serde(default)]
    consumed_at_ms: Option<i64>,
}

// Cross-platform home directory: $HOME on Unix, %USERPROFILE% on Windows.
// Returned as PathBuf so callers can .join() directly without re-parsing.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return format!("{}/{}", home.display(), rest);
        }
    }
    p.to_string()
}

// Active project root — resolved once at startup from a CLI arg
// (bram /path/to/project) or std::env::current_dir(). Read by
// the HTTP server, watcher, git/sessions/PTY commands.
struct ActiveProjectState(Mutex<PathBuf>);

// URLs for the two iframes.
//
// `tools` is always the internal loopback (Bram's own server,
// serving /__tools/index.html, /__shell/*, embedded assets, git/issues
// endpoints, etc.).
//
// `right_pane` is now always `tauri://localhost/__project/index.html`
// regardless of project configuration. The tauri:// scheme handler in
// `handle_tauri_scheme` intercepts paths under `/__project/*` and
// proxies them to `right_pane_upstream` (loopback default, or external
// dev server when `.bram.json` (or legacy `.xmlui-desktop.json`) declares one). Net effect: the
// right-pane iframe is same-origin with the shell, while the actual
// bytes still come from the upstream.
//
// `right_pane_upstream` is the proxy target (always ends with `/`).
//
// Service workers (used by MSW and xmlui's apiInterceptor) require a
// secure-context origin. Custom URI schemes are not secure contexts on
// macOS, so SW capability is lost in the right-pane iframe on macOS.
// Acceptable for normal apps; playground apps that synthesize APIs
// will not work in xd on macOS — see worklist item
// `same-origin-iframe-via-tauri-scheme-proxy` for the full discussion.
struct PaneUrlsState(Mutex<PaneUrls>);

#[derive(Default, Clone)]
struct PaneUrls {
    right_pane: String,
    tools: String,
    // Loopback-served URL used when no project server is declared. Used by
    // the agent-tools drawer's right-pane-info display (so the user sees
    // the actual upstream URL, not the tauri:// proxy URL) and as the
    // fallback upstream after the server block is removed from
    // project config at runtime.
    default_right_pane: String,
    // Base URL the tauri:// scheme handler proxies right-pane requests to.
    // Always ends with `/`. Switches between the loopback default and an
    // external server based on project config at startup and on
    // config reload.
    right_pane_upstream: String,
    // Always the internal-loopback base URL (ends with `/`), regardless of
    // any external dev-server declared in project config. Used by the
    // scheme handler to route xd-internal `/__*` requests (sessions,
    // worklist, app-info, etc.) — these never live on the project's dev
    // server even when one is declared.
    loopback_origin: String,
}

// Project-level config read from .bram.json at the project root, with
// legacy .xmlui-desktop.json accepted as a migration alias. Distinct
// from XMLUI's own config.json (the app-under-test isn't necessarily
// an XMLUI app). All fields optional.
#[derive(Default, Clone, serde::Deserialize)]
struct ProjectConfig {
    #[serde(default)]
    server: Option<ServerConfig>,
    #[serde(default)]
    shell: Option<ShellConfig>,
    #[serde(default)]
    worklist: Option<WorklistConfig>,
    #[serde(default)]
    ui: Option<UiConfig>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct ServerConfig {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    port: u16,
    #[serde(default = "default_server_path")]
    path: String,
}

// Optional shell-startup block. `agent` is a single command string
// typed verbatim into the bash prompt right after pty_spawn — bash
// parses it, so flags work: `claude --continue`, `codex resume`, etc.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct ShellConfig {
    #[serde(default)]
    agent: Option<String>,
}

#[derive(Default, Clone, serde::Deserialize)]
struct WorklistConfig {
    #[serde(default, rename = "batchCommitActions")]
    batch_commit_actions: Option<bool>,
}

// Optional UI block. Currently only `targetAppMinimized`, which drives
// the right-column h-splitter from the parent shell — see app/main.js.
#[derive(Default, Clone, serde::Deserialize)]
struct UiConfig {
    #[serde(default, rename = "targetAppMinimized")]
    target_app_minimized: Option<bool>,
}

fn default_server_path() -> String {
    "/".to_string()
}

// Lifecycle owner for an optional project-server child spawned per
// project config. Killed on ExitRequested, or on hot-reload when the
// declared command/cwd/port changes. Carries the spawn-time config so the
// reload path can diff against the new file and decide whether to respawn.
struct SpawnedServer {
    child: std::process::Child,
    config: ServerConfig,
}

#[derive(Default)]
struct SpawnedServerState(Mutex<Option<SpawnedServer>>);

// Windows' canonicalize() returns `\\?\C:\…` extended-length paths.
// PowerShell tolerates them, but cmd.exe child processes don't ("UNC paths
// are not supported. Defaulting to Windows directory.") — and silent
// fallback to %WINDIR% means any tool that resolves its workspace from
// cwd ends up rooted in C:\Windows. Strip the prefix unless this is a
// genuine UNC path (`\\?\UNC\server\share\…`), which must keep it.
fn strip_unc_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.starts_with("UNC\\") {
            return PathBuf::from(rest);
        }
    }
    p
}

// Flatten a filesystem path into a filename-safe identifier. On Linux/macOS
// this matches Claude Code's `~/.claude/projects/<encoded>/` scheme
// (`/Users/foo` → `-Users-foo`). On Windows we also fold `\` and `:` so
// `C:\Users\foo` becomes `C--Users-foo`; this is a conservative encoding
// for our own files (agent-hint), but for claude_sessions_dir it's a
// best-effort guess at Claude Code's Windows scheme — adjust if confirmed.
fn encode_path_for_filename(p: &std::path::Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            c => c,
        })
        .collect()
}

fn determine_project_root() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let candidate: PathBuf = if args.len() >= 2 && !args[1].starts_with('-') {
        PathBuf::from(&args[1])
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };
    let canonical = candidate.canonicalize().unwrap_or(candidate);
    strip_unc_prefix(canonical)
}

fn parse_cli_flags() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return;
    }
    match args[1].as_str() {
        "-h" | "--help" => {
            println!(
                "Usage: bram [PROJECT_DIR]\n\n\
                 Tauri shell that pairs a terminal with an XMLUI surface.\n\n\
                 Arguments:\n  \
                   [PROJECT_DIR]    Path to the XMLUI project to load (defaults to current directory)\n\n\
                 Options:\n  \
                   -h, --help       Print this help and exit\n  \
                   -V, --version    Print version and exit"
            );
            std::process::exit(0);
        }
        "-V" | "--version" => {
            println!("bram {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        s if s.starts_with('-') => {
            eprintln!("bram: unknown option '{}'", s);
            eprintln!("Try 'bram --help' for more information.");
            std::process::exit(1);
        }
        _ => {}
    }
}

fn first_nonempty_env(names: &[&str]) -> Option<String> {
    for name in names {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

// --- Comms-path trace foundation -----------------------------------------
//
// Process-global toggle for the comms-path trace log
// (`resources/bram-traces/bram-trace.log`, with prior runs archived at
// startup as `resources/bram-traces/bram-trace-YYYY-MM-DD*.log`). Set once at startup by
// `init_bram_trace_from_env`; every potential waypoint checks
// `bram_trace_enabled()` (a single atomic load) before doing any work,
// so the cost when off is essentially zero. Spec:
// https://github.com/judell/bram/issues/49#issuecomment-4524234976
//
// This commit installs the foundation only. NO call sites are wired up
// yet — `append_bram_trace_line` is intentionally unused. Subsequent
// commits add one trace category at a time, per the spec's verification
// discipline.
static BRAM_TRACE_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// Defer tools-pane-reload while a cycle is active (refs #93).
// Set when the watcher would otherwise emit during sentinel-active.
// Cleared and flushed once the sentinel is cleared. Single boolean
// so N watcher events during one cycle coalesce into one post-cycle
// reload — intended behavior.
static PENDING_TOOLS_RELOAD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static STARTUP_RUN_TRACE_EMITTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// Cached open handle for the live trace file. Lazy-init on first
// write: truncate-open, emit the session-start line, store the handle.
// Subsequent writes reuse the handle, dropping the per-event cost from
// open + write + close (3 syscalls) to write (1 syscall). Refs #82
// trace-cache-file-handle. Replaces the previous BRAM_TRACE_FIRST_WRITE
// flag — `guard.is_none()` IS the "first write" check now.
static BRAM_TRACE_FILE: std::sync::OnceLock<Mutex<Option<std::fs::File>>> =
    std::sync::OnceLock::new();

fn bram_trace_file_cell() -> &'static Mutex<Option<std::fs::File>> {
    BRAM_TRACE_FILE.get_or_init(|| Mutex::new(None))
}

fn bram_trace_enabled() -> bool {
    BRAM_TRACE_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

fn init_bram_trace_from_env() {
    let on = std::env::var("BRAM_TRACE")
        .map(|v| {
            let s = v.trim();
            s == "1" || s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes")
        })
        .unwrap_or(false);
    BRAM_TRACE_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
    if on {
        eprintln!(
            "[bram-trace] enabled (BRAM_TRACE=1); live trace destination: <project_root>/resources/bram-traces/bram-trace.log; previous runs archived at startup as <project_root>/resources/bram-traces/bram-trace-YYYY-MM-DD*.log"
        );
    }
}

#[allow(dead_code)]
fn bram_trace_log_file<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_resource_path(app, "bram-traces/bram-trace.log")
}

fn is_bram_trace_archive_rel(rel: &str) -> bool {
    rel.starts_with("resources/bram-traces/bram-trace-") && rel.ends_with(".log")
}

fn bram_trace_date_stamp_local() -> String {
    #[cfg(unix)]
    {
        if let Ok(out) = std::process::Command::new("date").arg("+%Y-%m-%d").output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(out) = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "(Get-Date).ToString('yyyy-MM-dd')",
            ])
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
        }
    }
    let secs = unix_now_ms() / 1000;
    let days = secs.div_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}", y, mo, d)
}

fn next_bram_trace_archive_path(active_path: &Path) -> Option<PathBuf> {
    let parent = active_path.parent()?;
    let stamp = bram_trace_date_stamp_local();
    for n in 1.. {
        let name = if n == 1 {
            format!("bram-trace-{}.log", stamp)
        } else {
            format!("bram-trace-{}-{}.log", stamp, n)
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn prepare_bram_trace_log<R: tauri::Runtime>(app: &AppHandle<R>) {
    if !bram_trace_enabled() {
        return;
    }
    let Some(active_path) = bram_trace_log_file(app) else {
        return;
    };
    if let Some(parent) = active_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "[bram-trace] failed to create trace directory {}: {}",
                parent.display(),
                e
            );
            return;
        }
    }

    let had_prior_log = std::fs::metadata(&active_path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if had_prior_log {
        let Some(archive_path) = next_bram_trace_archive_path(&active_path) else {
            eprintln!("[bram-trace] failed to choose archive path for previous live log");
            return;
        };
        let archived = std::fs::rename(&active_path, &archive_path).or_else(|rename_err| {
            std::fs::copy(&active_path, &archive_path)
                .map(|_| ())
                .map_err(|copy_err| {
                    std::io::Error::other(format!(
                        "rename failed: {}; copy failed: {}",
                        rename_err, copy_err
                    ))
                })
        });
        match archived {
            Ok(()) => {
                eprintln!(
                    "[bram-trace] archived previous live log to {}",
                    archive_path.display()
                );
            }
            Err(e) => {
                eprintln!(
                    "[bram-trace] failed to archive previous live log {}: {}",
                    active_path.display(),
                    e
                );
                return;
            }
        }
    }

    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&active_path)
    {
        eprintln!(
            "[bram-trace] failed to create fresh live log {}: {}",
            active_path.display(),
            e
        );
    }
}

// Single write API for every trace category. Format:
//   [<ISO-8601-UTC>] [<category>] <body>
//
// No-op when the toggle is off. Truncates the file on first call per
// session and writes a `[bram-trace] event=session-start pid=<pid>`
// header before the first real record.
//
// `category` is one of the closed-vocabulary tokens from the spec
// (`bram-trace`, `pty-out`, `pty-in`, `pty-menu`, `auth-record`,
// `watcher`, `emit`, `route`, `hook`, `iframe`). The function does not
// validate — the caller is expected to use a token from the spec, and
// `category` is checked in code review, not at runtime.
fn append_bram_trace_line<R: tauri::Runtime>(app: &AppHandle<R>, category: &str, body: &str) {
    if !bram_trace_enabled() {
        return;
    }
    let Some(path) = bram_trace_log_file(app) else {
        return;
    };
    let Ok(mut guard) = bram_trace_file_cell().lock() else {
        return;
    };
    // First write of the session: ensure the parent dir exists, open
    // in append mode, write the session-start line, then cache the
    // handle. Every subsequent write reuses the cached handle and
    // pays only a single `write(2)` per line — measurably keeps the
    // PTY read loop responsive under heavy TUI animation. Refs #82.
    //
    // Append mode is load-bearing: subprocess hooks (e.g. .claude/hooks/
    // worklist-guard.py) also append to this file via plain open("a").
    // Without O_APPEND on the host's cached fd, the host's position-
    // tracked writeln! would silently overwrite hook bytes appended
    // since the host's last write. That was the root cause of #69
    // (after May 24, no [hook] records appeared in the trace log even
    // though the hook was firing and BRAM_TRACE/BRAM_TRACE_LOG were
    // both set — the hook's appends were being clobbered).
    //
    // prepare_bram_trace_log already archives the prior session's log
    // and creates a fresh empty live log at startup, so .truncate(true)
    // here is redundant and incompatible with append mode anyway.
    if guard.is_none() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);
        let Ok(mut file) = opened else {
            return;
        };
        let _ = writeln!(
            file,
            "[{}] [bram-trace] event=session-start pid={}",
            format_iso_utc_ms(unix_now_ms()),
            std::process::id()
        );
        *guard = Some(file);
    }
    if let Some(file) = guard.as_mut() {
        let _ = writeln!(
            file,
            "[{}] [{}] {}",
            format_iso_utc_ms(unix_now_ms()),
            category,
            body
        );
    }
}

// Render a short, single-line, escape-aware preview of `data` capped at
// `max` chars. Shared by every trace category that surfaces raw bytes
// (currently `[pty-out]`; future: `[pty-in]`, `[route]` request body).
// Control characters render as `\xNN`, common whitespace gets the usual
// escapes, and the output is double-quoted so a trailing space or
// trailing escape doesn't get visually merged with adjacent text.
fn bram_trace_preview(data: &str, max: usize) -> String {
    let mut out = String::with_capacity(max + 8);
    out.push('"');
    let mut count = 0usize;
    for ch in data.chars() {
        if count >= max {
            out.push_str("...");
            break;
        }
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x1b' => out.push_str("\\x1b"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
        count += 1;
    }
    out.push('"');
    out
}

// True when the normalized turn submission begins with one of the
// structured-intent prefixes recognized by `parse_worklist_authorization_message`
// (`approved:`, `drop:`) or by chat-side convention (`iterate:`, `talk:`).
// Used by `[pty-out]` to flag candidate authorization writes; the
// authoritative parse still happens in `record_worklist_authorization_from_input`.
fn is_structured_intent_prefix(data: &str) -> bool {
    let n = normalize_turn_submission(data);
    let s = n.as_str();
    s.starts_with("approved:")
        || s.starts_with("drop:")
        || s.starts_with("iterate:")
        || s.starts_with("talk:")
}

// Trace a host → iframe Tauri emit with no payload. Spec fields:
// kind=<event name>, payload_size=0, correlation_id=<empty for now>.
// Place a call immediately above each unit-payload `emit` site.
fn trace_emit_signal<R: tauri::Runtime>(app: &AppHandle<R>, kind: &str) {
    if !bram_trace_enabled() {
        return;
    }
    append_bram_trace_line(
        app,
        "emit",
        &format!("kind={} payload_size=0 correlation_id=", kind),
    );
}

// Trace a host → iframe Tauri emit with a payload. Serializes the
// payload to JSON to count bytes; matches what Tauri itself does on the
// wire, modulo MessagePack vs JSON depending on Tauri version (the
// byte count here is the JSON form, used as a proxy for payload scale).
fn trace_emit_payload<R: tauri::Runtime, S: serde::Serialize>(
    app: &AppHandle<R>,
    kind: &str,
    payload: &S,
) {
    if !bram_trace_enabled() {
        return;
    }
    let size = serde_json::to_vec(payload).map(|v| v.len()).unwrap_or(0);
    append_bram_trace_line(
        app,
        "emit",
        &format!("kind={} payload_size={} correlation_id=", kind, size),
    );
}

#[derive(Clone)]
struct LastTauriEvent {
    payload: serde_json::Value,
    ts_ms: i64,
}

fn last_tauri_events_cell() -> &'static Mutex<HashMap<String, LastTauriEvent>> {
    static CELL: OnceLock<Mutex<HashMap<String, LastTauriEvent>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_tauri_event(event_name: &str, payload: serde_json::Value) {
    if let Ok(mut guard) = last_tauri_events_cell().lock() {
        guard.insert(
            event_name.to_string(),
            LastTauriEvent {
                payload,
                ts_ms: unix_now_ms(),
            },
        );
    }
}

fn emit_replayable_signal<R: tauri::Runtime>(app: &AppHandle<R>, event_name: &str) {
    trace_emit_signal(app, event_name);
    remember_tauri_event(event_name, serde_json::Value::Null);
    // #150 regression instrumentation: every remember is observable, so a
    // post-restart `event-replay-skip` for the same event tells us the
    // store didn't persist (i.e., the route that should have re-emitted
    // before the iframe attached hadn't fired yet, or didn't fire at all).
    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "event-remember",
            &format!("kind={} payload_size=4 source=signal", event_name),
        );
    }
    let _ = app.emit(event_name, ());
}

fn emit_replayable_payload<R, P>(app: &AppHandle<R>, event_name: &str, payload: P)
where
    R: tauri::Runtime,
    P: serde::Serialize + Clone,
{
    trace_emit_payload(app, event_name, &payload);
    let replay_payload = serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null);
    let payload_size = serde_json::to_vec(&replay_payload)
        .map(|v| v.len())
        .unwrap_or(0);
    remember_tauri_event(event_name, replay_payload);
    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "event-remember",
            &format!(
                "kind={} payload_size={} source=payload",
                event_name, payload_size
            ),
        );
    }
    let _ = app.emit(event_name, payload);
}

fn latest_tauri_event_response(event_name: &str) -> serde_json::Value {
    let found = last_tauri_events_cell()
        .lock()
        .ok()
        .and_then(|guard| guard.get(event_name).cloned());
    match found {
        Some(ev) => serde_json::json!({
            "ok": true,
            "exists": true,
            "name": event_name,
            "tsMs": ev.ts_ms,
            "payload": ev.payload,
        }),
        None => serde_json::json!({
            "ok": true,
            "exists": false,
            "name": event_name,
        }),
    }
}

fn replay_tauri_event_live<R: tauri::Runtime>(
    app: &AppHandle<R>,
    event_name: &str,
) -> serde_json::Value {
    let found = last_tauri_events_cell()
        .lock()
        .ok()
        .and_then(|guard| guard.get(event_name).cloned());
    let Some(ev) = found else {
        // #150 regression instrumentation. Pairs with the
        // `event-remember` trace: a replay-skip for an event the host
        // emitted earlier in the session is the smoking gun for a #150
        // regression. A replay-skip with no prior remember is harmless
        // (the iframe asked speculatively and there was nothing to
        // replay yet).
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "event-replay-skip",
                &format!("kind={} reason=no-stored-payload", event_name),
            );
        }
        return serde_json::json!({
            "ok": true,
            "exists": false,
            "name": event_name,
            "replayed": false,
        });
    };
    let payload_size = serde_json::to_vec(&ev.payload)
        .map(|v| v.len())
        .unwrap_or(0);
    append_bram_trace_line(
        app,
        "event-replay",
        &format!(
            "kind={} ts_ms={} payload_size={}",
            event_name, ev.ts_ms, payload_size
        ),
    );
    let emit_result = app.emit(event_name, ev.payload.clone());
    serde_json::json!({
        "ok": emit_result.is_ok(),
        "exists": true,
        "name": event_name,
        "replayed": emit_result.is_ok(),
        "tsMs": ev.ts_ms,
        "error": emit_result.err().map(|e| e.to_string()),
    })
}

// Process-local sequence number for [route] correlation ids. Combined
// with the entry timestamp it disambiguates two concurrent requests
// that arrive in the same millisecond.
static ROUTE_TRACE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_route_correlation_id() -> String {
    let n = ROUTE_TRACE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("route-{}-{}", unix_now_ms(), n)
}

// Trace an HTTP route entry. Generated by handle_http for every
// inbound request before any work happens, so the entry timestamp is
// the receive moment (not the post-dispatch moment).
fn trace_route_entry<R: tauri::Runtime>(
    app: &AppHandle<R>,
    method: &str,
    path: &str,
    query: &str,
    correlation_id: &str,
) {
    if !bram_trace_enabled() {
        return;
    }
    append_bram_trace_line(
        app,
        "route",
        &format!(
            "phase=entry method={} path={} query={} correlation_id={}",
            method, path, query, correlation_id
        ),
    );
}

// Map a notify::EventKind into a short label for the [watcher] trace.
// Keep the vocabulary small and stable so analysis scripts can rely on
// it (`create`, `modify`, `remove`, `access`, `other`).
fn notify_event_kind_label(kind: &notify::EventKind) -> &'static str {
    use notify::EventKind;
    match kind {
        EventKind::Create(_) => "create",
        EventKind::Modify(_) => "modify",
        EventKind::Remove(_) => "remove",
        EventKind::Access(_) => "access",
        _ => "other",
    }
}

// Trace an HTTP route exit. Logged just before the response is written
// back to the socket so `duration_ms` is the full host-side handling
// time. `body_size` is the response body's byte length, not the wire
// length (which would include HTTP headers).
fn trace_route_exit<R: tauri::Runtime>(
    app: &AppHandle<R>,
    method: &str,
    path: &str,
    correlation_id: &str,
    status: u16,
    body_size: usize,
    duration_ms: u128,
) {
    if !bram_trace_enabled() {
        return;
    }
    append_bram_trace_line(
        app,
        "route",
        &format!(
            "phase=exit method={} path={} status={} body_size={} duration_ms={} correlation_id={}",
            method, path, status, body_size, duration_ms, correlation_id
        ),
    );
}

// --- End comms-path trace foundation -------------------------------------

fn project_root<R: tauri::Runtime>(app: Option<&AppHandle<R>>) -> Option<PathBuf> {
    if let Some(a) = app {
        if let Some(state) = a.try_state::<ActiveProjectState>() {
            if let Ok(p) = state.0.lock() {
                if !p.as_os_str().is_empty() {
                    return Some(p.clone());
                }
            }
        }
    }
    // Fallback for code paths that run before .manage() (rare; mostly
    // setup-time helpers). Old behavior.
    resolve_app_root(app).and_then(|app_root| app_root.parent().map(|p| p.to_path_buf()))
}

fn git_run<R: tauri::Runtime>(app: &AppHandle<R>, args: &[&str]) -> Result<String, String> {
    let root = project_root(Some(app)).ok_or_else(|| "no project root".to_string())?;
    let out = std::process::Command::new("git")
        .current_dir(&root)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// Update-availability check. Fetched once from GitHub at first request and
// cached for the process lifetime — repeat hits are cheap and we don't
// want to thrash the API's 60/hr anonymous limit. `XMLUI_DESKTOP_FAKE_CURRENT`
// substitutes the env value for CARGO_PKG_VERSION so the banner UI can be
// exercised ahead of an actual release: `XMLUI_DESKTOP_FAKE_CURRENT=0.0.1 cargo run`.
#[derive(serde::Serialize, Clone)]
struct AppInfo {
    current: String,
    latest: Option<String>,
    has_update: bool,
    release_url: Option<String>,
}

static APP_INFO_CACHE: OnceLock<Mutex<Option<AppInfo>>> = OnceLock::new();

// Cache of the "currently live" Claude session JSONL path and its mtime.
// Updated by latest_claude_session_path with hysteresis so the choice
// doesn't oscillate when claude touches an old session file.
static LIVE_CLAUDE_SESSION: OnceLock<Mutex<Option<(PathBuf, std::time::SystemTime)>>> =
    OnceLock::new();
static LIVE_CODEX_SESSION: OnceLock<Mutex<Option<(PathBuf, std::time::SystemTime)>>> =
    OnceLock::new();

// Real-time detection of claude's permission menu by tapping the PTY
// byte stream. Necessary because the JSONL file lags claude's actual
// state by ~10-20s (claude buffers a turn's records and flushes as one
// batch), so /__sessions/latest-pending can never see a tool_use until
// after the user has already responded. The PTY is the only signal
// available *while* the menu is on screen.
//
// PTY_TAIL holds a rolling ~8KB of the most recent PTY output bytes.
// PTY_MENU holds the currently-displayed menu (or None). Updated from
// the pty_spawn reader thread; cleared on pty_write (any user input
// dismisses the menu).
// PTY_MENU_SUPPRESSED records (tool, when_cleared) for ~2s after a
// user-driven dismissal. Without it, the next PTY chunk arrives, the
// detector re-scans the rolling buffer, and the *just-dismissed* menu
// text is still in the buffer — so the detector re-fires and the agent
// pane shows the menu briefly after click. Suppression breaks that
// loop until either time elapses or a *different* tool's menu appears.
static PTY_TAIL: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();
static PTY_MENU: OnceLock<Mutex<Option<PtyMenu>>> = OnceLock::new();
static PTY_MENU_SUPPRESSED: OnceLock<Mutex<Option<(String, std::time::Instant)>>> = OnceLock::new();
// Grace window for the "buffer briefly evicted a still-live menu" case.
// When `pty_menu_detect` returns None but `PTY_MENU` is currently Some,
// we defer the dismiss emit for MENU_EVICTION_GRACE_MS and re-check on
// the next pty_menu_update cycle. Re-detection within the window
// suppresses both the dismiss and the re-show; grace expiry without
// re-detection emits the dismiss normally. Refs #77 menu-detector
// stabilization.
static PTY_MENU_EVICTION_GRACE: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();
const MENU_EVICTION_GRACE_MS: u128 = 350;

// The loopback HTTP server's port, captured at setup. Used to inject
// XMLUI_DESKTOP_PORT into the PTY child's environment so the agent can
// reach /__worklist/resolve and other /__* routes without rediscovering
// the random port each turn.
static LOOPBACK_PORT: OnceLock<u16> = OnceLock::new();
static LOOPBACK_STARTED_MS: OnceLock<i64> = OnceLock::new();

#[derive(serde::Serialize, Clone)]
struct MenuOption {
    key: String,
    label: String,
}

#[derive(serde::Serialize, Clone)]
struct PtyMenu {
    tool: String,
    text: String,
    options: Vec<MenuOption>,
    #[serde(rename = "toolCallSignature", skip_serializing_if = "Option::is_none")]
    tool_call_signature: Option<String>,
    #[serde(rename = "toolCallDiff", skip_serializing_if = "Option::is_none")]
    tool_call_diff: Option<String>,
    // Origin of the current `tool_call_signature` for trace introspection.
    // Not serialized to the iframe — purely a host-side bookkeeping field
    // surfaced via the [pty-menu] state=... trace lines.
    // "pty" — produced by `extract_pty_tool_call` from the rendered
    //         `⏺ <Tool>(...)` line in the PTY tail.
    // "jsonl" — produced by `lookup_pending_tool_call` from the Claude
    //          session JSONL (initial detect or recheck).
    // None — signature is currently None.
    // Refs #170 follow-up.
    #[serde(skip)]
    signature_source: Option<&'static str>,
}

// Parse `1. Foo` / `❯1. Foo` / `❯ 2. Bar` lines out of the captured menu
// text. Cursor marker `❯` (U+276F) may or may not lead the first option;
// subsequent options have a numeric prefix only. Stops at the first non-
// matching line after at least one option has been collected so trailing
// PTY chatter doesn't get swept in. Returns an empty Vec on no matches —
// the UI falls back to its hardcoded buttons in that case.
fn parse_menu_options(text: &str) -> Vec<MenuOption> {
    let mut opts: Vec<MenuOption> = Vec::new();
    for raw in text.lines() {
        // Strip leading cursor marker (with optional space / NBSP) and whitespace.
        let mut trimmed =
            raw.trim_start_matches(|c: char| c == '❯' || c == '\u{a0}' || c.is_whitespace());
        // If the cursor glyph appears mid-line — e.g. ANSI strip collapsed
        // the "Do you want to proceed? ❯1. Yes" prompt onto one line —
        // restart parsing from the cursor position so option 1 doesn't
        // get swallowed as pre-menu chatter. Observed live at
        // 2026-06-02 ~04:42Z: panel rendered options 2 and 3 but missing
        // 1 because option 1's line had the menu header in front of it.
        // Refs #170.
        if let Some(cursor_idx) = trimmed.find('❯') {
            trimmed = trimmed[cursor_idx..]
                .trim_start_matches(|c: char| c == '❯' || c == '\u{a0}' || c.is_whitespace());
        }
        if trimmed.is_empty() {
            // Blank line terminates after at least one option — the PTY emits
            // an empty line between the option list and end-of-menu footer
            // ("Esc to cancel · Tab to amend …").
            if !opts.is_empty() {
                break;
            }
            continue;
        }
        // Match leading digit(s) followed by "." and a label.
        let (digits, rest) = match trimmed.find('.') {
            Some(idx) => trimmed.split_at(idx),
            None => (trimmed, ""),
        };
        let is_numbered =
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) && !rest.is_empty();
        if !is_numbered {
            // Non-numbered, non-empty line. If we have a previous option,
            // treat it as a wrapped continuation of that option's label —
            // strip_ansi removes the cursor-positioning escapes Claude
            // Code uses for indentation, so we can't rely on leading
            // whitespace to distinguish continuation lines from
            // post-menu chatter. Empty lines (above) are the terminator.
            if let Some(last) = opts.last_mut() {
                last.label.push(' ');
                last.label.push_str(trimmed);
            }
            // Pre-menu chatter (no options yet) just gets skipped.
            continue;
        }
        let label = rest.trim_start_matches('.').trim();
        if label.is_empty() {
            if !opts.is_empty() {
                break;
            }
            continue;
        }
        opts.push(MenuOption {
            key: digits.to_string(),
            label: label.to_string(),
        });
    }
    opts
}

// PtyMenu equality compares only `tool` — `text` carries surrounding
// PTY bytes captured by position (`pos1 - 200`..`pos2 + 200`), which
// shifts as the rolling 8 KB tail evolves even when the user-visible
// menu is unchanged. Comparing text would defeat dedup-on-emit and
// produce bursty `state=shown` re-emits for the same on-screen prompt.
// Refs #77 tighten-pty-menu-emit-cadence.
impl PartialEq for PtyMenu {
    fn eq(&self, other: &Self) -> bool {
        // Compare `tool` and the parsed option count. Text is intentionally
        // excluded (it tracks PTY-tail position and would defeat dedup), but
        // option count changes are real user-visible state — a first emit
        // before options were buffered (`options=0`) followed by a re-detect
        // with options populated should fire a fresh `state=shown` so the UI
        // gets the parsed labels.
        self.tool == other.tool
            && self.options.len() == other.options.len()
            && self.tool_call_signature.is_some() == other.tool_call_signature.is_some()
            && self.tool_call_diff.is_some() == other.tool_call_diff.is_some()
    }
}
impl Eq for PtyMenu {}

// Sentinel for "menu detected via `❯1.` cursor bytes but the
// `Do you want to use X?` header text has not yet landed in the
// rolling buffer". First detection cycle stores this state but does
// NOT emit `state=shown`; the second cycle either resolves a real
// tool name from the now-buffered header or falls back to the
// pre-menu grep. Race observed when a user-input dismissal arrives
// between the cursor's PTY chunk and the header's PTY chunk, which
// previously caused `tool=Bash` to leak from a prior prompt onto a
// Read prompt's shown emit. Refs #77 tighten-pty-menu-emit-cadence.
const PENDING_TOOL: &str = "<pending>";

fn pty_tail_cell() -> &'static Mutex<Vec<u8>> {
    PTY_TAIL.get_or_init(|| Mutex::new(Vec::with_capacity(8192)))
}

// Cached snapshot of the Claude PTY's rotating status line ("Meandering…
// (1m 53s · ↓ 5.8k tokens)"). Surfaced via /__agent-status and the
// `agent-status-changed` Tauri event so the Worklist tab can render a
// live status row without the user having to look at the terminal.
// Refs the worklist item `claude-status-row-in-worklist`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
struct AgentStatus {
    provider: Option<String>,
    state: String,
    verb: Option<String>,
    #[serde(rename = "elapsedText")]
    elapsed_text: Option<String>,
    // Sub-state phrase parsed from inside the status-line parens
    // (e.g. "thinking some more", "almost done thinking"). Optional.
    substate: Option<String>,
    // Retained in the struct so older callers / consumers don't break,
    // but no longer populated — we dropped the token column from the
    // row per user request after live-testing showed JSONL token counts
    // lagged Claude's live status display by tens of seconds when
    // assistant deltas hadn't been flushed yet.
    #[serde(rename = "outputTokensText", skip_serializing_if = "Option::is_none")]
    output_tokens_text: Option<String>,
    #[serde(rename = "updatedAtMs")]
    updated_at_ms: i64,
}

impl AgentStatus {
    fn idle() -> Self {
        AgentStatus {
            provider: None,
            state: "idle".to_string(),
            verb: None,
            elapsed_text: None,
            substate: None,
            output_tokens_text: None,
            updated_at_ms: 0,
        }
    }
}

static AGENT_STATUS: OnceLock<Mutex<AgentStatus>> = OnceLock::new();
fn agent_status_cell() -> &'static Mutex<AgentStatus> {
    AGENT_STATUS.get_or_init(|| Mutex::new(AgentStatus::idle()))
}

const CODEX_AGENT_STATUS_STALE_MS: i64 = 3_000;

// Parsed snapshot of Claude's rotating status line. Drives the row's
// verb, elapsed text, and substate ("thinking some more", "almost done
// thinking"). Tokens dropped per user request — the live PTY count was
// fine but the JSONL fallback lagged by tens of seconds, and the
// resulting mismatch was worse than showing nothing.
struct ParsedStatusLine {
    verb: String,
    elapsed_text: String,
    substate: Option<String>,
}

fn is_claude_status_verb_char(ch: char) -> bool {
    ch.is_alphabetic() || ch == '\''
}

fn is_valid_claude_status_verb(verb: &str) -> bool {
    if verb.is_empty() || verb.len() > 40 || verb.chars().count() < 4 {
        return false;
    }
    let Some(first) = verb.chars().next() else {
        return false;
    };
    if !first.is_uppercase() || !verb.chars().all(is_claude_status_verb_char) {
        return false;
    }
    // Strict title-case: only the first char may be uppercase. Rejects
    // glitches like "GScurriying" where a stray CSI-G column-position
    // escape leaked a literal G into the walk-back. CC's verbs are
    // simple gerunds (Hashing, Frying, Architecting) — always one cap.
    if verb.chars().skip(1).any(|c| c.is_uppercase()) {
        return false;
    }
    true
}

// Parse Claude's full status line from an ANSI-stripped PTY tail.
// Anchors on the rightmost "… (" form. Inside the parens, splits on the
// "·" separator: index 0 is elapsed (always present); remaining segments
// are either "↓ N tokens" (skipped) or substate phrases. Returns None
// when the anchor is missing — typically when heavy output has evicted
// the status line from the 8 KB rolling tail. Unicode-aware to accept
// accented verbs ("Flambéing").
fn parse_claude_status_line(stripped_tail: &[u8]) -> Option<ParsedStatusLine> {
    let s = std::str::from_utf8(stripped_tail).ok()?;
    let anchor = s.rfind("… (")?;
    let after_open = anchor + "… (".len();
    let rel_close = s[after_open..].find(')')?;
    let inside = &s[after_open..after_open + rel_close];

    // Split inside-parens on "·"; trim each segment.
    let segs: Vec<String> = inside
        .split('·')
        .map(|seg| seg.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if segs.is_empty() {
        return None;
    }
    let elapsed_text = segs[0].clone();
    // Elapsed sanity: must contain a digit and a time unit.
    if !elapsed_text.chars().any(|c| c.is_ascii_digit())
        || !elapsed_text.chars().any(|c| matches!(c, 'h' | 'm' | 's'))
    {
        return None;
    }

    // Substate is the first segment after elapsed that doesn't start
    // with a token-count arrow ("↓" output, "↑" input — both come and
    // go on the status line and are skipped here).
    let is_token_seg = |seg: &str| seg.starts_with('↓') || seg.starts_with('↑');
    let substate = segs.iter().skip(1).find(|seg| !is_token_seg(seg)).cloned();

    // Walk back from "…" to find the verb. Unicode-aware.
    let before = s[..anchor].trim_end();
    let mut verb_start_byte = 0usize;
    for (idx, ch) in before.char_indices().rev() {
        if !is_claude_status_verb_char(ch) {
            verb_start_byte = idx + ch.len_utf8();
            break;
        }
    }
    let verb = before[verb_start_byte..].to_string();
    // Reject mid-paint partials. CC's gerund verbs are always 5+ chars
    // (Hashing, Frying, Undulating, Flibbertigibbeting…). A 2–3 char
    // capture is almost certainly the parser sampling the tail between
    // two repaints, catching only the leading bytes of the next verb.
    if !is_valid_claude_status_verb(&verb) {
        return None;
    }

    Some(ParsedStatusLine {
        verb,
        elapsed_text,
        substate,
    })
}

// Verb-only fallback: handles the standalone form CC writes briefly at
// turn start (just "Verb…" with no elapsed parens) and during rapid
// repaints where the trailing "(...)" group hasn't landed in the tail
// yet. Anchors on the rightmost lone "…" and walks back for the verb.
// Unicode-aware. Skips matches that are immediately followed by " ("
// since those are the full-form case already handled by
// parse_claude_status_line.
fn parse_claude_status_verb_only(stripped_tail: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(stripped_tail).ok()?;
    // Walk from the rightmost "…" and try each; pick the first one that
    // yields a valid verb and isn't part of the full-form "… (".
    let mut search_end = s.len();
    while let Some(rel) = s[..search_end].rfind('…') {
        let after = &s[rel + '…'.len_utf8()..];
        // Skip full-form occurrences (parse_claude_status_line handles those).
        let is_full_form = after.starts_with(' ') && after.trim_start().starts_with('(');
        if is_full_form {
            search_end = rel;
            continue;
        }
        let before = s[..rel].trim_end();
        let mut verb_start_byte = 0usize;
        for (idx, ch) in before.char_indices().rev() {
            if !is_claude_status_verb_char(ch) {
                verb_start_byte = idx + ch.len_utf8();
                break;
            }
        }
        let verb = before[verb_start_byte..].to_string();
        if !is_valid_claude_status_verb(&verb) {
            search_end = rel;
            continue;
        }
        return Some(verb);
    }
    None
}

// Sticky-verb cache. Holds the most recently parsed verb so that when
// the status line gets evicted from the PTY tail for a few seconds, the
// row keeps showing "Hashing…" instead of flickering to generic
// "working…" and back. STICKY_VERB_TTL_MS bounds how long we'll keep
// showing a stale verb — long enough to ride through typical eviction
// bursts (single Read of a big file), short enough that a turn that
// genuinely moved on doesn't keep showing the old verb forever.
const STICKY_VERB_TTL_MS: i64 = 15_000;

struct StickyVerb {
    verb: String,
    captured_at_ms: i64,
}

static STICKY_VERB_CELL: OnceLock<Mutex<Option<StickyVerb>>> = OnceLock::new();
fn sticky_verb_cell() -> &'static Mutex<Option<StickyVerb>> {
    STICKY_VERB_CELL.get_or_init(|| Mutex::new(None))
}

fn sticky_verb_remember(verb: &str) {
    if let Ok(mut guard) = sticky_verb_cell().lock() {
        *guard = Some(StickyVerb {
            verb: verb.to_string(),
            captured_at_ms: unix_now_ms(),
        });
    }
}

fn sticky_verb_recall() -> Option<String> {
    let now = unix_now_ms();
    sticky_verb_cell().lock().ok().and_then(|guard| {
        guard
            .as_ref()
            .filter(|s| now - s.captured_at_ms <= STICKY_VERB_TTL_MS)
            .map(|s| s.verb.clone())
    })
}

fn sticky_verb_clear() {
    if let Ok(mut guard) = sticky_verb_cell().lock() {
        *guard = None;
    }
}

// Sticky substate cache. Same shape as sticky_verb but shorter TTL —
// substate ("thinking some more", "almost done thinking") shifts faster
// than the verb, so a 5 s window covers brief PTY-tail evictions while
// not pinning a phrase that's actually become stale.
const STICKY_SUBSTATE_TTL_MS: i64 = 5_000;

struct StickySubstate {
    substate: String,
    captured_at_ms: i64,
}

static STICKY_SUBSTATE_CELL: OnceLock<Mutex<Option<StickySubstate>>> = OnceLock::new();
fn sticky_substate_cell() -> &'static Mutex<Option<StickySubstate>> {
    STICKY_SUBSTATE_CELL.get_or_init(|| Mutex::new(None))
}

fn sticky_substate_remember(s: &str) {
    if let Ok(mut guard) = sticky_substate_cell().lock() {
        *guard = Some(StickySubstate {
            substate: s.to_string(),
            captured_at_ms: unix_now_ms(),
        });
    }
}

fn sticky_substate_recall() -> Option<String> {
    let now = unix_now_ms();
    sticky_substate_cell().lock().ok().and_then(|guard| {
        guard
            .as_ref()
            .filter(|s| now - s.captured_at_ms <= STICKY_SUBSTATE_TTL_MS)
            .map(|s| s.substate.clone())
    })
}

fn sticky_substate_clear() {
    if let Ok(mut guard) = sticky_substate_cell().lock() {
        *guard = None;
    }
}

// Parsed CC natural-end banner ("* Brewed for 30s", "* Worked for 1m
// 2s"). Carries the past-tense verb and the duration text so the
// status row can briefly show "Brewed · 30s" as a finished cue
// instead of just disappearing when the turn ends.
struct ParsedEndBanner {
    verb: String,
    duration_text: String,
}

// Detect CC's natural-end banner in the stripped PTY tail. Format:
// "* <PastTenseVerb> for <duration>" (e.g. "* Worked for 1m 2s",
// "* Brewed for 12s", "* Sautéed for 45s"). When present, this is the
// most authoritative "turn just ended" signal — stronger than silence,
// independent of menu / escape state. pty_agent_status_update uses it
// to end the turn even within the post-escape suppression window.
//
// Returns the parsed verb + duration so callers can surface them as a
// "finished" cue. Last (rightmost-in-tail) match wins.
fn parse_claude_natural_end_banner(stripped: &[u8]) -> Option<ParsedEndBanner> {
    let s = std::str::from_utf8(stripped).unwrap_or("");
    // CC's banner prefix glyph varies (`*`, `✻`, `✶`, `✳`, …) and the
    // exact whitespace differs (strip_ansi inserts spaces in place of
    // CSI G column-position escapes). Skip the prefix concern entirely:
    // scan each line for the substring " for " and check that the
    // word just before is a single capitalized past-tense-looking verb
    // and the word just after is a duration.
    let mut best: Option<ParsedEndBanner> = None;
    for line in s.lines() {
        let Some(idx) = line.find(" for ") else {
            continue;
        };
        // Verb: walk back from idx over alphabetic chars; reject if no
        // verb or not Capitalized-then-lowercase.
        let before = &line[..idx];
        let verb_start = before
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphabetic())
            .last()
            .map(|(i, _)| i);
        let Some(vstart) = verb_start else { continue };
        let verb = &before[vstart..];
        if verb.chars().count() < 4 {
            continue;
        }
        let mut cs = verb.chars();
        let Some(first) = cs.next() else { continue };
        if !first.is_uppercase() || !cs.all(|c| c.is_lowercase()) {
            continue;
        }
        // Duration: first non-whitespace char after " for " must be a
        // digit. Accept a run of digits / 'h' / 'm' / 's' / spaces;
        // stop at anything else (e.g. " · 5 messages" trailing the
        // duration on some CC builds).
        let after = line[idx + " for ".len()..].trim_start();
        if !after
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            continue;
        }
        let mut end = 0usize;
        for (i, c) in after.char_indices() {
            if c.is_ascii_digit() || matches!(c, 'h' | 'm' | 's') || c == ' ' {
                end = i + c.len_utf8();
            } else {
                break;
            }
        }
        // If extraction stopped mid-word (next char is alphabetic),
        // we grabbed only the leading digit + an incidental h/m/s
        // letter from prose like "Listed for 5 minutes" or
        // "30 cups". Reject — real banner durations are followed by
        // whitespace, punctuation (·), or end-of-line.
        if after[end..]
            .chars()
            .next()
            .map(|c| c.is_alphabetic())
            .unwrap_or(false)
        {
            continue;
        }
        let candidate = after[..end].trim();
        if candidate.is_empty() {
            continue;
        }
        // strip_ansi inserts a single space for each CSI G column-
        // positioning escape. CC's TUI paints multi-unit durations
        // like "1m 22s" piecewise — e.g. "1m 2\x1b[19Gs" — which
        // arrives here as "1m 2 s". Strip internal whitespace and
        // re-tokenize as (\d+)([hms])+. Lossy for the split-unit
        // case ("1m 22s" reconstructs as "1m 2s" because the second
        // "2" lived in the prior screen state we can't recover) but
        // good enough that the row shows the verb instead of
        // freezing on the previous turn's cue.
        let compact: String = candidate.chars().filter(|c| !c.is_whitespace()).collect();
        let mut tokens: Vec<String> = Vec::new();
        let mut current_digits = String::new();
        let mut valid = true;
        for c in compact.chars() {
            if c.is_ascii_digit() {
                current_digits.push(c);
            } else if matches!(c, 'h' | 'm' | 's') && !current_digits.is_empty() {
                let mut token = std::mem::take(&mut current_digits);
                token.push(c);
                tokens.push(token);
            } else {
                valid = false;
                break;
            }
        }
        if !valid || !current_digits.is_empty() || tokens.is_empty() {
            continue;
        }
        let duration_text = tokens.join(" ");
        best = Some(ParsedEndBanner {
            verb: verb.to_string(),
            duration_text,
        });
    }
    best
}

// Scan the stripped PTY tail for known CC thinking-phase phrases. CC's
// TUI paints the status line piecewise with column-positioning ANSI
// escapes, so the full "(elapsed · ↓ tokens · substate)" group rarely
// arrives contiguously in our 8 KB tail; the substate phrase itself
// almost always does. Order matters: try the longer / more specific
// phrases first, and on ties pick the rightmost (most recent) match
// by comparing the END position of each match. Word-boundary check
// rejects substring matches like "rethinking" or "extended-thinking".
fn parse_substate_phrase_scan(stripped: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(stripped).ok()?;
    const PHRASES: &[&str] = &[
        "almost done thinking",
        "thinking some more",
        "thinking even more",
        "thinking more",
        "still thinking",
        "thinking",
    ];
    let bytes = s.as_bytes();
    let mut best: Option<(usize, &str)> = None;
    for &p in PHRASES {
        if let Some(pos) = s.rfind(p) {
            // Left word boundary: start of string, whitespace, or "·".
            let left_ok = pos == 0 || {
                let prev = bytes[pos - 1];
                prev.is_ascii_whitespace() || prev == b'\xb7' // U+00B7 ·
            };
            if !left_ok {
                continue;
            }
            // Right word boundary: end of string, whitespace, ")", or
            // start of digit (e.g. "thought for 3s" — accept digit).
            let end = pos + p.len();
            let right_ok = end >= bytes.len() || {
                let next = bytes[end];
                next.is_ascii_whitespace() || next == b')' || next.is_ascii_digit()
            };
            if !right_ok {
                continue;
            }
            // Pick the match whose END is rightmost — covers the case
            // where "thinking" appears inside "almost done thinking" and
            // we want the longer phrase.
            if best.map(|(b_end, _)| end > b_end).unwrap_or(true) {
                best = Some((end, p));
            }
        }
    }
    let (_end, phrase) = best?;
    Some(phrase.to_string())
}

// Format elapsed milliseconds as "1m 53s" / "23s" / "1h 5m" matching
// the Claude Code TUI status line cosmetic.
fn format_elapsed_text(ms: i64) -> String {
    let total_s = (ms / 1000).max(0);
    let h = total_s / 3600;
    let m = (total_s % 3600) / 60;
    let s = total_s % 60;
    if h > 0 {
        format!("{}h {}m", h, m)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

// Stats for the current Claude turn, derived from the session JSONL tail
// by a background poller (see start_claude_turn_stats_poll). Cached so
// the PTY reader thread can read them without touching the filesystem
// or doing JSON parsing — keeping that thread fast is what keeps CC's
// startup terminal-capability queries from timing out and triggering
// verbose-resume mode (the "transcript replay on launch" regression).
//
// `user_ts_ms` is the timestamp of the most recent real user message;
// elapsed = now - user_ts_ms is computed at read time so the row's
// elapsed display advances continuously between poll cycles.
#[derive(Clone)]
struct ClaudeTurnStats {
    user_ts_ms: i64,
    is_working: bool,
}

static CLAUDE_TURN_STATS_CACHE: OnceLock<Mutex<Option<ClaudeTurnStats>>> = OnceLock::new();
fn claude_turn_stats_cell() -> &'static Mutex<Option<ClaudeTurnStats>> {
    CLAUDE_TURN_STATS_CACHE.get_or_init(|| Mutex::new(None))
}

// Timestamp of the most recent agent-status clear-to-idle. The PTY tail
// still contains the just-ended turn's status line for a beat after the
// clear, so subsequent PTY chunks would re-emit "working" by parsing
// that stale line. pty_agent_status_update gates on user_ts_ms >
// last_cleared_at to refuse stale resurrection until JSONL records a
// fresh user-turn boundary.
static LAST_CLEARED_AT_MS: OnceLock<Mutex<i64>> = OnceLock::new();
fn last_cleared_at_cell() -> &'static Mutex<i64> {
    LAST_CLEARED_AT_MS.get_or_init(|| Mutex::new(0))
}

// `user_ts_ms` of the most recent turn killed by an explicit user cancel
// (Esc / pty-escape). Distinct from `last_cleared_at_ms` because cancel
// is a hard signal: JSONL never records `end_turn` for a canceled turn,
// so cache.is_working stays true forever and the 500ms soft-clear gate
// can't tell the row to stay down. The kill stamp keeps the row
// suppressed for the exact turn that was canceled; the next real user
// message lands a new user_ts_ms that bypasses the gate.
static LAST_KILLED_USER_TS_MS: OnceLock<Mutex<i64>> = OnceLock::new();
fn last_killed_user_ts_ms_cell() -> &'static Mutex<i64> {
    LAST_KILLED_USER_TS_MS.get_or_init(|| Mutex::new(0))
}

// Timestamp of the most recent pty-escape. Used to suppress the
// silence-based hard kill for a window after Esc — after Esc the user
// is in control and may pause to think before continuing; silence in
// that window doesn't mean the turn ended.
static LAST_PTY_ESCAPE_MS: OnceLock<Mutex<i64>> = OnceLock::new();
fn last_pty_escape_ms_cell() -> &'static Mutex<i64> {
    LAST_PTY_ESCAPE_MS.get_or_init(|| Mutex::new(0))
}
const POST_ESCAPE_NO_KILL_WINDOW_MS: i64 = 15_000;

// user_ts_ms of the most recent turn whose start the natural-end banner
// gate has observed. When pty_agent_status_update sees a fresh
// stats.user_ts_ms (advance past this stamp), it drains pty_tail_cell so
// the prior turn's "* <Verb>ed for X" banner — still sitting in the
// rolling 8KB buffer — cannot falsely match
// parse_claude_natural_end_banner and kill the status row mid-turn.
// Refs #178.
static LAST_BANNER_USER_TS_MS: OnceLock<Mutex<i64>> = OnceLock::new();
fn last_banner_user_ts_ms_cell() -> &'static Mutex<i64> {
    LAST_BANNER_USER_TS_MS.get_or_init(|| Mutex::new(0))
}

fn kill_current_claude_turn<R: tauri::Runtime>(app: &AppHandle<R>) {
    kill_current_claude_turn_with_finished(app, None, None);
}

// End the current turn. The row briefly displays a "finished" cue
// (verb + duration when known, generic otherwise) before fading to
// idle, so the GUI signals turn completion explicitly rather than
// silently removing the row. Callers that have a parsed end-banner
// pass its verb/duration; silence-detected and user-cancel paths
// pass None.
fn kill_current_claude_turn_with_finished<R: tauri::Runtime>(
    app: &AppHandle<R>,
    verb: Option<String>,
    elapsed_text: Option<String>,
) {
    let current_ts = claude_turn_stats_cell()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.user_ts_ms))
        .unwrap_or_else(unix_now_ms);
    if let Ok(mut g) = last_killed_user_ts_ms_cell().lock() {
        *g = current_ts;
    }
    let provider = match hinted_session_provider(app) {
        Some(SessionProvider::Codex) => "codex",
        _ => "claude",
    };
    agent_status_emit_finished(app, provider, verb, elapsed_text);
    // Dedicated "real end-of-turn" signal — distinct from the eager
    // `agent-turn-end` event which fires on any 800 ms silence. The
    // worklist's awaitingResponse gate keys on this; agent-turn-end
    // would clear it on every inter-burst pause.
    trace_emit_signal(app, "agent-turn-killed");
    let _ = app.emit("agent-turn-killed", ());
}

// Emit a "finished" agent-status payload. Stamps the cleared-at cursor
// and drops sticky verb/substate so a stale PTY tail can't re-emit
// "working" after the turn has ended.
fn agent_status_emit_finished<R: tauri::Runtime>(
    app: &AppHandle<R>,
    provider: &str,
    verb: Option<String>,
    elapsed_text: Option<String>,
) {
    let next = AgentStatus {
        provider: Some(provider.to_string()),
        state: "finished".to_string(),
        verb,
        elapsed_text,
        substate: None,
        output_tokens_text: None,
        updated_at_ms: unix_now_ms(),
    };
    // Idempotency: if the cached state already matches the proposed
    // finished payload, skip the side effects. Without this, the same
    // banner still sitting in the PTY tail re-fires on every PTY chunk
    // during the race between the user submitting a new turn and the
    // JSONL poller updating stats.user_ts_ms. Each re-fire re-stamped
    // cleared_at, which then made the cleared_at race-window check at
    // the top of pty_agent_status_update suppress the next turn's
    // working emit. Net effect: the finished cue (e.g. "Cogitated ·
    // 30s") got stuck for the duration of multiple subsequent turns.
    let changed = if let Ok(cur) = agent_status_cell().lock() {
        cur.state != next.state
            || cur.provider != next.provider
            || cur.verb != next.verb
            || cur.elapsed_text != next.elapsed_text
    } else {
        true
    };
    if !changed {
        return;
    }
    if let Ok(mut g) = last_cleared_at_cell().lock() {
        *g = unix_now_ms();
    }
    sticky_verb_clear();
    sticky_substate_clear();
    if let Ok(mut cur) = agent_status_cell().lock() {
        *cur = next.clone();
    }
    emit_replayable_payload(app, "agent-status-changed", &next);
}

// Background poller: re-reads the latest Claude session JSONL every
// 300 ms and refreshes the cache. Started once at app setup; runs for
// the process lifetime. Skips work when the active provider isn't
// Claude — cheap branch, no file I/O.
fn start_claude_turn_stats_poll<R: tauri::Runtime>(app_handle: AppHandle<R>) {
    // Stamp the kill cursor at launch so any in-progress turn that
    // didn't naturally end before bram restarted (last assistant !=
    // end_turn) doesn't surface a stale "working… 2m 18s" row on the
    // very first poll. Only user_ts_ms values from messages submitted
    // in this bram session will exceed the stamp and light the row.
    if let Ok(mut g) = last_killed_user_ts_ms_cell().lock() {
        *g = unix_now_ms();
    }
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(300));
        if !matches!(
            hinted_session_provider(&app_handle),
            Some(SessionProvider::Claude)
        ) {
            // Clear cache when Claude isn't active so the row doesn't
            // surface stale data after a provider switch.
            if let Ok(mut guard) = claude_turn_stats_cell().lock() {
                *guard = None;
            }
            continue;
        }
        let next = match read_latest_session_tail(&app_handle, Some(SessionProvider::Claude), 300) {
            Ok(tail) => compute_claude_turn_stats(&tail),
            Err(_) => None,
        };
        if let Ok(mut guard) = claude_turn_stats_cell().lock() {
            *guard = next;
        }
    });
}

// Walk the latest session JSONL tail backwards to find the most recent
// real user message (NOT a tool_result), then sum output_tokens across
// all assistant messages after that boundary. Marks the turn as
// "working" unless the most recent assistant record after the boundary
// reports stop_reason "end_turn".
//
// Returns None when no real user message is found in the tail (e.g.
// session not yet started, or tail too short to contain one).
fn compute_claude_turn_stats(tail: &[u8]) -> Option<ClaudeTurnStats> {
    // Parse JSONL lines newest-first. Looking for: the most recent real
    // user message (turn boundary), stop_reason of latest assistant
    // (decides is_working).
    let text = std::str::from_utf8(tail).ok()?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut user_ts_ms: Option<i64> = None;
    let mut latest_assistant_stop: Option<String> = None;
    let mut saw_assistant_after_user = false;
    for line in lines.iter().rev() {
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "user" => {
                let is_tool_result = rec
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|first| first.get("type"))
                    .and_then(|t| t.as_str())
                    .map(|t| t == "tool_result")
                    .unwrap_or(false);
                if is_tool_result {
                    continue;
                }
                let ts = rec
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .and_then(parse_iso_to_ms);
                user_ts_ms = ts;
                break;
            }
            "assistant" => {
                if latest_assistant_stop.is_none() {
                    latest_assistant_stop = rec
                        .get("message")
                        .and_then(|m| m.get("stop_reason"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                saw_assistant_after_user = true;
            }
            _ => {} // sidecar metadata: ignore
        }
    }
    let user_ts_ms = user_ts_ms?;
    // "working" unless the most recent assistant record explicitly
    // ended the turn. If no assistant has spoken yet, also working
    // (turn started, agent hasn't yielded).
    let ended = latest_assistant_stop.as_deref() == Some("end_turn");
    let is_working = !ended || !saw_assistant_after_user;
    Some(ClaudeTurnStats {
        user_ts_ms,
        is_working,
    })
}

// Minimal ISO-8601 to unix-ms parser for Claude session timestamps
// (e.g. "2026-06-02T22:53:42.123Z"). Returns None on parse failure.
// Hand-rolled rather than pulled from chrono so we don't add a dep
// for one parse call.
fn parse_iso_to_ms(iso: &str) -> Option<i64> {
    let b = iso.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year: i64 = iso.get(0..4)?.parse().ok()?;
    let month: u32 = iso.get(5..7)?.parse().ok()?;
    let day: u32 = iso.get(8..10)?.parse().ok()?;
    let hour: u32 = iso.get(11..13)?.parse().ok()?;
    let min: u32 = iso.get(14..16)?.parse().ok()?;
    let sec: u32 = iso.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut ms_frac: i64 = 0;
    if iso.len() > 19 && b[19] == b'.' {
        let frac_end = iso[20..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| 20 + i)
            .unwrap_or(iso.len());
        let frac_str = &iso[20..frac_end];
        if !frac_str.is_empty() {
            let take = frac_str.len().min(3);
            ms_frac = frac_str[..take].parse().unwrap_or(0);
            for _ in take..3 {
                ms_frac *= 10;
            }
        }
    }
    fn is_leap(y: i64) -> bool {
        (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
    }
    let mut days: i64 = 0;
    let start_year = if year >= 1970 { 1970 } else { return None };
    for y in start_year..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    let days_in_month: [i64; 12] = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for m in 1..month as usize {
        days += days_in_month[m - 1];
    }
    days += day as i64 - 1;
    let total_sec = days * 86400 + (hour as i64) * 3600 + (min as i64) * 60 + sec as i64;
    Some(total_sec * 1000 + ms_frac)
}

// Update the agent-status cache from a fresh PTY chunk. Gated on the
// active provider being Claude (Codex has a different TUI shape and is
// out of scope for v1). Re-uses the rolling pty_tail_cell so we don't
// need a second buffer. Emits `agent-status-changed` only when the
// snapshot actually changes; the working→finished transition is
// driven from the JSONL boundary detector — see
// emit_claude_finished_from_jsonl_boundary. This function only emits
// working-state updates (verb / elapsed / substate). Refs #179.
fn pty_agent_status_update<R: tauri::Runtime>(app: &AppHandle<R>) {
    let active_provider = hinted_session_provider(app);
    if !matches!(active_provider, Some(SessionProvider::Claude)) {
        return;
    }
    let stats = match claude_turn_stats_cell().lock() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    };
    let Some(stats) = stats else { return };
    let cleared_at = last_cleared_at_cell().lock().map(|g| *g).unwrap_or(0);
    let since_clear = unix_now_ms() - cleared_at;
    // Suppress only during the brief race window after a clear-to-idle
    // (poller hasn't yet refreshed the cache from JSONL). After the
    // window, trust the cache: if it still says is_working=true, the
    // clear was spurious (e.g. permission-menu pause misread by the
    // turn-end silence detector) and the row should reappear.
    const CLEARED_AT_RACE_WINDOW_MS: i64 = 500;
    if stats.user_ts_ms <= cleared_at && since_clear < CLEARED_AT_RACE_WINDOW_MS {
        return;
    }
    // Hard kill from a user cancel (pty-escape). Stays in effect until
    // the user submits a new prompt — JSONL won't record an end_turn
    // for a canceled turn, so the cache-based working signal alone
    // can't tell the row to stay down.
    let killed_ts = last_killed_user_ts_ms_cell()
        .lock()
        .map(|g| *g)
        .unwrap_or(0);
    if stats.user_ts_ms <= killed_ts {
        return;
    }
    // On fresh user_ts_ms, drain pty_tail_cell so the prior turn's
    // status-line text still in the rolling 8KB buffer can't surface
    // as the new turn's working verb before genuine new PTY chunks
    // overwrite it. Refs #178.
    if let Ok(mut last) = last_banner_user_ts_ms_cell().lock() {
        if stats.user_ts_ms > *last {
            *last = stats.user_ts_ms;
            if let Ok(mut tail) = pty_tail_cell().lock() {
                tail.clear();
            }
        }
    }
    let stripped = pty_tail_cell().lock().ok().map(|tail| strip_ansi(&tail));
    // Working emit runs only while JSONL says the turn is active;
    // otherwise the stale status line still in the PTY tail would
    // re-emit "working". The finished transition is owned by the
    // JSONL boundary detector (emit_claude_finished_from_jsonl_boundary),
    // not by anything in this function.
    if !stats.is_working {
        return;
    }
    let full = stripped.as_ref().and_then(|s| parse_claude_status_line(s));
    // Phrase scan for substate — runs regardless of full-form parse,
    // because CC paints the status line piecewise and the substate
    // word usually arrives in the tail even when "… (" doesn't.
    let scanned_substate = stripped
        .as_ref()
        .and_then(|s| parse_substate_phrase_scan(s));
    if let Some(ref s) = scanned_substate {
        sticky_substate_remember(s);
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "status-substate",
                &format!("source=scan phrase={:?}", s),
            );
        }
    }

    let next = if let Some(p) = full {
        sticky_verb_remember(&p.verb);
        if let Some(ref s) = p.substate {
            sticky_substate_remember(s);
        }
        // Prefer parsed substate from the full-form (most precise),
        // then phrase-scan result, then sticky.
        let substate = p
            .substate
            .clone()
            .or_else(|| scanned_substate.clone())
            .or_else(sticky_substate_recall);
        AgentStatus {
            provider: Some("claude".to_string()),
            state: "working".to_string(),
            verb: Some(p.verb),
            elapsed_text: Some(p.elapsed_text),
            substate,
            // Tokens dropped per user request — distracting. The JSONL
            // sum and PTY parse still populate cache/parsed fields, but
            // we deliberately don't surface them on the row.
            output_tokens_text: None,
            updated_at_ms: unix_now_ms(),
        }
    } else {
        // Full-form parse missed. Try the standalone "Verb…" form
        // (early in turn, between repaints), then fall back to sticky.
        let verb_only = pty_tail_cell()
            .lock()
            .ok()
            .map(|tail| strip_ansi(&tail))
            .and_then(|stripped| parse_claude_status_verb_only(&stripped));
        if let Some(ref v) = verb_only {
            sticky_verb_remember(v);
        }
        let elapsed_ms = (unix_now_ms() - stats.user_ts_ms).max(0);
        AgentStatus {
            provider: Some("claude".to_string()),
            state: "working".to_string(),
            verb: verb_only.or_else(sticky_verb_recall),
            elapsed_text: Some(format_elapsed_text(elapsed_ms)),
            substate: scanned_substate.or_else(sticky_substate_recall),
            output_tokens_text: None,
            updated_at_ms: unix_now_ms(),
        }
    };

    let mut emit_payload: Option<AgentStatus> = None;
    if let Ok(mut cur) = agent_status_cell().lock() {
        let changed = cur.verb != next.verb
            || cur.elapsed_text != next.elapsed_text
            || cur.substate != next.substate
            || cur.state != next.state
            || cur.provider != next.provider;
        if changed {
            *cur = next.clone();
            emit_payload = Some(next);
        } else {
            cur.updated_at_ms = next.updated_at_ms;
        }
    }
    if let Some(payload) = emit_payload {
        emit_replayable_payload(app, "agent-status-changed", &payload);
    }
}

// PTY agent-turn state machine (issue #70, later extended for Codex
// parity in #74). Detects end-of-turn from spinner/activity glyphs in
// the PTY stream: while the agent is running, the terminal redraws
// every 100-200ms with a spinner-like glyph. When that activity stops,
// the next non-spinner PTY chunk (typically the prompt redraw) fires
// `agent-turn-end`. The iframe consumes that event for a fast inflight
// clear path, bypassing the multi-second JSONL flush chain.
struct AgentTurnState {
    last_spinner_at: Option<std::time::Instant>,
    is_active: bool,
    last_emit_at: Option<std::time::Instant>,
    // Set when is_active transitions false → true; cleared on the
    // reverse transition. Lets pty_agent_turn_update emit
    // `[spinner-period] state=ended duration_ms=<n>` carrying the
    // active-period length, so #78 analysis can see whether premature
    // turn-end fires correlate with short active periods. Refs #78.
    active_since: Option<std::time::Instant>,
}

fn agent_turn_state_cell() -> &'static Mutex<AgentTurnState> {
    static CELL: OnceLock<Mutex<AgentTurnState>> = OnceLock::new();
    CELL.get_or_init(|| {
        Mutex::new(AgentTurnState {
            last_spinner_at: None,
            is_active: false,
            last_emit_at: None,
            active_since: None,
        })
    })
}

// 800ms threshold: activity ticks are 100-200ms apart while thinking;
// 800ms of no spinner/activity reliably indicates the agent stopped
// updating.
const AGENT_TURN_IDLE_THRESHOLD_MS: u128 = 800;

// Sentinel-clear gate (#91 follow-up). The emit threshold above
// (~800ms) misfires on natural inter-burst pauses; a real
// end-of-turn typically shows silence >= 3s. clear_active_sentinel
// fires only when the silence-detected turn-end exceeds this gate,
// avoiding premature spinner clears mid-burst.
const MIN_SILENCE_FOR_SENTINEL_CLEAR_MS: u128 = 3000;

// Suppress repeat emits within 5s of the last one. After a real
// end-of-turn the TUIs sometimes re-pulse briefly (input-box re-render,
// scroll update, title refresh, etc.) — that re-arms the detector and
// would fire a second turn-end ~1s later. The cooldown keeps the trace
// clean and the iframe listener from doing extra no-op work. State
// transitions still happen (is_active flips); only the outbound emit is
// gated.
const AGENT_TURN_EMIT_COOLDOWN_MS: u128 = 5000;

fn should_clear_status_on_turn_activity_stop(
    silence_ms: Option<u128>,
    menu_pending: bool,
    post_escape: bool,
) -> bool {
    // The 800 ms turn-end event is intentionally eager for legacy
    // listeners, but it is too eager for the status row: Claude and
    // Codex both pause spinner/title repainting between tool/result
    // bursts. Only clear the row on a conservative silence window and
    // never while the permission panel or post-Esc recovery owns the UI.
    const ROW_HARD_KILL_MIN_SILENCE_MS: u128 = 10_000;
    silence_ms
        .map(|s| s >= ROW_HARD_KILL_MIN_SILENCE_MS)
        .unwrap_or(false)
        && !menu_pending
        && !post_escape
}

// Byte-level check for turn-activity glyphs without allocating a
// String. Asterisk family is U+2700..U+277F (UTF-8 prefix 0xE2 0x9C);
// braille patterns are U+2800..U+283F (prefix 0xE2 0xA0). In practice
// this covers both Claude's spinner redraws and Codex's braille/title
// activity updates. Middle dot U+00B7 is deliberately NOT matched — it
// appears in non-spinner TUI text (for example token-count separators)
// and would over-fire the detector.
fn pty_chunk_has_turn_activity_glyph(chunk: &[u8]) -> bool {
    for w in chunk.windows(2) {
        if w[0] == 0xE2 && (w[1] == 0x9C || w[1] == 0xA0) {
            return true;
        }
    }
    false
}

fn agent_status_set_codex_working<R: tauri::Runtime>(
    app: &AppHandle<R>,
    active_since: std::time::Instant,
) {
    if !matches!(hinted_session_provider(app), Some(SessionProvider::Codex)) {
        return;
    }
    let elapsed_ms = std::time::Instant::now()
        .saturating_duration_since(active_since)
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let next = AgentStatus {
        provider: Some("codex".to_string()),
        state: "working".to_string(),
        verb: Some("working".to_string()),
        elapsed_text: Some(format_elapsed_text(elapsed_ms)),
        substate: None,
        output_tokens_text: None,
        updated_at_ms: unix_now_ms(),
    };
    let mut emit_payload: Option<AgentStatus> = None;
    if let Ok(mut cur) = agent_status_cell().lock() {
        let changed = cur.verb != next.verb
            || cur.elapsed_text != next.elapsed_text
            || cur.substate != next.substate
            || cur.state != next.state
            || cur.provider != next.provider;
        if changed {
            *cur = next.clone();
            emit_payload = Some(next);
        } else {
            cur.updated_at_ms = next.updated_at_ms;
        }
    }
    if let Some(payload) = emit_payload {
        emit_replayable_payload(app, "agent-status-changed", &payload);
    }
}

fn pty_agent_turn_update<R: tauri::Runtime>(app: &AppHandle<R>, chunk: &[u8]) {
    let now = std::time::Instant::now();
    let has_spinner = pty_chunk_has_turn_activity_glyph(chunk);
    let mut emit_now = false;
    let mut spinner_started = false;
    let mut spinner_ended_duration_ms: Option<u128> = None;
    let mut turn_end_silence_ms: Option<u128> = None;
    let mut codex_active_since: Option<std::time::Instant> = None;
    if let Ok(mut state) = agent_turn_state_cell().lock() {
        if has_spinner {
            if !state.is_active {
                // false -> true transition: start of a spinner-active
                // period. Refs #78 detector instrumentation.
                spinner_started = true;
                state.active_since = Some(now);
            }
            state.last_spinner_at = Some(now);
            state.is_active = true;
            codex_active_since = state.active_since;
        } else if state.is_active {
            if let Some(last) = state.last_spinner_at {
                let silence = now.saturating_duration_since(last).as_millis();
                if silence > AGENT_TURN_IDLE_THRESHOLD_MS {
                    // true -> false transition: spinner-active period
                    // ends. Capture duration_ms (active_since → now)
                    // and silence_ms (last spinner glyph → now) for
                    // the [spinner-period] and [turn-end] traces.
                    // Refs #78 detector instrumentation.
                    if let Some(started) = state.active_since {
                        spinner_ended_duration_ms =
                            Some(now.saturating_duration_since(started).as_millis());
                    }
                    state.active_since = None;
                    state.is_active = false;
                    let in_cooldown = state.last_emit_at.map_or(false, |t| {
                        now.saturating_duration_since(t).as_millis() < AGENT_TURN_EMIT_COOLDOWN_MS
                    });
                    if !in_cooldown {
                        state.last_emit_at = Some(now);
                        emit_now = true;
                        turn_end_silence_ms = Some(silence);
                    }
                }
            }
        }
    }
    if let Some(active_since) = codex_active_since {
        agent_status_set_codex_working(app, active_since);
    }
    if bram_trace_enabled() {
        if spinner_started {
            append_bram_trace_line(app, "spinner-period", "state=started");
        }
        if let Some(d) = spinner_ended_duration_ms {
            append_bram_trace_line(
                app,
                "spinner-period",
                &format!("state=ended duration_ms={}", d),
            );
        }
    }
    if emit_now {
        if bram_trace_enabled() {
            // Include the silence gap that triggered the fire. A
            // premature fire (#78) typically shows silence_ms close to
            // AGENT_TURN_IDLE_THRESHOLD_MS (~800-1000); a real
            // end-of-turn fire shows much longer silence (5s+).
            let silence_str = turn_end_silence_ms
                .map(|s| format!(" silence_ms={}", s))
                .unwrap_or_default();
            append_bram_trace_line(
                app,
                "turn-end",
                &format!("source=pty-turn-activity-stop{}", silence_str),
            );
        }
        trace_emit_signal(app, "agent-turn-end");
        let _ = app.emit("agent-turn-end", ());
        // Hard-kill only when (a) silence exceeds the sentinel-clear
        // threshold (>= 3 s = real end-of-turn) AND (b) no permission
        // menu is up. A slow menu answer trivially exceeds 3 s of
        // silence; hard-killing on that loses the row for the rest of
        // the turn once the user finally answers. The menu surface
        // shows "awaiting your input" via pendingMenu independently.
        let menu_pending = pty_menu_cell()
            .lock()
            .ok()
            .map(|g| g.is_some())
            .unwrap_or(false);
        // Recently after Esc the user is in control — silence is them
        // thinking, not the turn ending. Suppress the hard kill for
        // 15s after an Esc so the row survives think pauses. Double-Esc
        // still hard-kills via the pty-escape handler.
        let last_escape = last_pty_escape_ms_cell().lock().map(|g| *g).unwrap_or(0);
        let post_escape =
            last_escape > 0 && unix_now_ms() - last_escape < POST_ESCAPE_NO_KILL_WINDOW_MS;
        if should_clear_status_on_turn_activity_stop(turn_end_silence_ms, menu_pending, post_escape)
        {
            kill_current_claude_turn(app);
        } else {
            if bram_trace_enabled() {
                append_bram_trace_line(
                    app,
                    "agent-status",
                    &format!(
                        "op=skip-clear source=pty-turn-activity-stop silence_ms={} menu_pending={} post_escape={}",
                        turn_end_silence_ms.unwrap_or(0),
                        menu_pending,
                        post_escape
                    ),
                );
            }
        }

        // Host-side guarantee: clear any active inflight sentinel when
        // the agent's turn truly ends. Gated on a higher silence
        // threshold than the emit threshold — premature fires
        // (silence ~800-1000ms during inter-burst pauses) would clear
        // the spinner mid-turn; real turn-ends show silence_ms
        // >= 3000ms (typically much more after the emit cooldown).
        // Refs #91 follow-up. Agents have trouble making
        // /__worklist/end or /__iterate/end their literal LAST action
        // — they tend to acknowledge tool results with one more
        // sentence. Tying the sentinel-clear to silence-detected
        // turn-end removes that risk class. The explicit end routes
        // still work for agents that want to clear early.
        if turn_end_silence_ms.map_or(false, |s| s >= MIN_SILENCE_FOR_SENTINEL_CLEAR_MS) {
            let claimed_after = inflight_claim_ids_and_claimed_at(app)
                .map(|(ids, claimed_at)| !ids.is_empty() && unix_now_ms() >= claimed_at)
                .unwrap_or(false);
            record_turn_completion_monitor(
                "pty-silence",
                "pty",
                "silence-threshold",
                format!(
                    "PTY turn activity stopped after {} ms of silence",
                    turn_end_silence_ms.unwrap_or(0)
                ),
                unix_now_ms(),
                claimed_after,
            );
            clear_active_sentinel(app);
        }
    }
}

fn pty_menu_cell() -> &'static Mutex<Option<PtyMenu>> {
    PTY_MENU.get_or_init(|| Mutex::new(None))
}

fn pty_menu_suppressed_cell() -> &'static Mutex<Option<(String, std::time::Instant)>> {
    PTY_MENU_SUPPRESSED.get_or_init(|| Mutex::new(None))
}

fn pty_menu_eviction_grace_cell() -> &'static Mutex<Option<std::time::Instant>> {
    PTY_MENU_EVICTION_GRACE.get_or_init(|| Mutex::new(None))
}

// Update the menu detection state with a fresh chunk of PTY output.
// Maintains a rolling 8KB tail buffer; checks for claude's menu signature
// ("1. Yes" + "2. Yes" within proximity); transitions PTY_MENU accordingly.
// Logs every state transition to stderr so failures-to-render can be
// correlated against actual detector activity.
fn pty_menu_update<R: tauri::Runtime>(app: &AppHandle<R>, chunk: &[u8]) {
    let tail_cell = pty_tail_cell();
    let mut tail = match tail_cell.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    tail.extend_from_slice(chunk);
    if tail.len() > 8192 {
        let drop = tail.len() - 8192;
        tail.drain(..drop);
        // After a mid-buffer trim, advance to the next newline so the
        // tail does not start mid-escape. strip_ansi reads only
        // complete CSI/OSC escapes; partial leading escape bytes
        // (e.g. "5Gthought" left after dropping the first 3 bytes of
        // "\x1b[35Gthought") would survive as literal text and corrupt
        // downstream parsers — notably parse_claude_natural_end_banner,
        // which would then walk back across "Gthought" and treat the
        // leading "G" as a capitalized verb prefix, falsely matching
        // "Gthought for 4s" and emitting it as a finished verb.
        // Newlines never appear inside ANSI escapes, so they are a
        // safe resync boundary.
        if let Some(nl_pos) = tail.iter().position(|&b| b == b'\n') {
            tail.drain(..=nl_pos);
        }
    }
    let codex_action_required_title = pty_codex_action_required_pos(&tail).is_some();
    let mut detected = pty_menu_detect(&tail);
    drop(tail);

    let mut signature_trace: Option<(bool, &'static str, usize, usize)> = None;
    if let Some(menu) = detected.as_mut() {
        let lookup = lookup_pending_tool_call(app);
        let signature_chars = lookup
            .signature
            .as_ref()
            .map(|s| s.chars().count())
            .unwrap_or(0);
        signature_trace = Some((
            lookup.signature.is_some(),
            lookup.reason,
            signature_chars,
            lookup.tail_bytes,
        ));
        // PTY-preferred: if `pty_menu_detect` already populated a
        // signature from the rendered `⏺ <Tool>(...)` tail line, keep
        // it. Use JSONL only when PTY missed. Diff is always JSONL —
        // it's an enrichment field, not a fallback. Refs #170 follow-up.
        if menu.tool_call_signature.is_none() {
            if let Some(jsonl_sig) = lookup.signature {
                menu.tool_call_signature = Some(jsonl_sig);
                menu.signature_source = Some("jsonl");
            }
        }
        menu.tool_call_diff = lookup.diff;
    }

    // Post-click suppression: if the user just dismissed a menu for
    // tool X, ignore detections of tool X for ~2s. The just-dismissed
    // menu's text is still sitting in PTY_TAIL.
    if let Some(ref new_menu) = detected {
        if let Ok(suppressed) = pty_menu_suppressed_cell().lock() {
            if let Some((suppressed_tool, when)) = suppressed.as_ref() {
                if suppressed_tool == &new_menu.tool
                    && when.elapsed() < std::time::Duration::from_secs(2)
                {
                    eprintln!(
                        "[pty-menu] suppressed re-detection of tool={} ({}ms after dismissal)",
                        new_menu.tool,
                        when.elapsed().as_millis()
                    );
                    detected = None;
                }
            }
        }
    }

    // Compute transition + apply under lock, then emit outside the lock.
    // Emitting under the lock would risk deadlock if listeners synchronously
    // call back into pty_menu_cell (they don't today, but cheap to avoid).
    let mut emit_payload: Option<Option<PtyMenu>> = None;
    let mut recheck_target: Option<String> = None;

    if let Ok(mut menu) = pty_menu_cell().lock() {
        let prev_menu = menu.as_ref().cloned();

        // Codex keeps a permission prompt alive by refreshing the raw
        // terminal title (`Action Required | bram`) even when no new
        // option text is emitted. The numbered choices can therefore
        // scroll out of PTY_TAIL while the prompt is still on screen.
        // Keep the last shown menu until user input clears it or the
        // title heartbeat stops.
        if detected.is_none() && codex_action_required_title {
            if let Some(prev) = prev_menu.as_ref() {
                if prev.tool != PENDING_TOOL {
                    detected = Some(prev.clone());
                }
            }
        }

        // Sticky-against-eviction guard. If detection returned None but
        // a menu was previously shown, defer the dismiss emit for up
        // to MENU_EVICTION_GRACE_MS. Re-detection within the window
        // suppresses both the dismiss and the re-show — the menu was
        // just briefly hidden in the rolling buffer behind intervening
        // TUI noise. The user-input dismissal path (`pty_menu_clear`)
        // is independent and unaffected. Refs #77 menu-detector
        // stabilization.
        if detected.is_none() && prev_menu.is_some() {
            if let Ok(mut grace) = pty_menu_eviction_grace_cell().lock() {
                match *grace {
                    None => {
                        *grace = Some(std::time::Instant::now());
                        eprintln!(
                            "[pty-menu] eviction grace started for tool={}",
                            prev_menu.as_ref().map(|p| p.tool.as_str()).unwrap_or("?")
                        );
                        return;
                    }
                    Some(started) if started.elapsed().as_millis() < MENU_EVICTION_GRACE_MS => {
                        return;
                    }
                    Some(started) => {
                        *grace = None;
                        eprintln!(
                            "[pty-menu] eviction grace expired ({}ms); proceeding with dismiss",
                            started.elapsed().as_millis()
                        );
                    }
                }
            }
        } else if detected.is_some() {
            if let Ok(mut grace) = pty_menu_eviction_grace_cell().lock() {
                if grace.is_some() {
                    eprintln!("[pty-menu] eviction grace cleared by re-detection");
                    *grace = None;
                }
            }
        }

        // Don't downgrade a known menu to pending. If detection returned
        // a pending menu but the previous cycle had a definitive tool
        // name, this is just the rolling buffer drifting the header out
        // of the captured `text` window — the on-screen menu hasn't
        // changed. Carry the previous tool forward. Refs #77
        // tighten-pty-menu-emit-cadence.
        if let (Some(d), Some(p)) = (detected.as_mut(), prev_menu.as_ref()) {
            if d.tool == PENDING_TOOL && p.tool != PENDING_TOOL {
                d.tool = p.tool.clone();
            }
        }

        // Symmetric: don't downgrade a known menu's options from a
        // parsed count back to 0 for the same tool. Same rolling-
        // buffer drift cause as the tool-name carry-forward above —
        // a later pty_menu_detect cycle anchors on `❯1.` but the
        // 200-byte `text` window no longer contains all option lines,
        // so parse_menu_options returns []. Without this guard the
        // iframe sees options=0, pendingMenu.options.length === 0
        // evaluates true, and the panel falls back to the hardcoded
        // 3-button HStack instead of the #169 dynamic vertical
        // layout. Empirically observed on the 02:56:09Z Write menu
        // test for #170. If a later cycle genuinely re-parses options
        // for the same tool, PartialEq lets it through; this only
        // carries forward when the new detection would degrade.
        // Refs #170.
        if let (Some(d), Some(p)) = (detected.as_mut(), prev_menu.as_ref()) {
            if d.options.is_empty() && !p.options.is_empty() && d.tool == p.tool {
                d.options = p.options.clone();
            }
        }

        let state_changed = prev_menu.as_ref() != detected.as_ref();
        // First-cycle pending: store the state so the next detect cycle
        // can see we've already waited one, but suppress the `shown`
        // emit and trace until the tool name resolves. Refs #77.
        let detected_is_pending = matches!(detected.as_ref(), Some(d) if d.tool == PENDING_TOOL);
        let should_emit_change = state_changed && !detected_is_pending;

        match (&prev_menu, &detected) {
            (None, Some(nm)) => eprintln!("[pty-menu] None -> Some(tool={})", nm.tool),
            (Some(o), Some(nm)) if o != nm => {
                eprintln!("[pty-menu] Some(tool={}) -> Some(tool={})", o.tool, nm.tool)
            }
            (Some(o), None) => {
                eprintln!("[pty-menu] Some(tool={}) -> None (buffer evicted)", o.tool)
            }
            _ => {}
        }

        if should_emit_change && bram_trace_enabled() {
            // Structured [pty-menu] trace, distinct from the operator
            // -facing eprintln! above. `reason=byte-pattern` for shows,
            // `reason=buffer-evicted` when the detector lost the
            // pattern out of PTY_TAIL without the user dismissing it.
            // Explicit user dismissals get their own trace from
            // pty_menu_clear with reason=user-input.
            match (&detected, &prev_menu) {
                (Some(nm), _) => append_bram_trace_line(
                    app,
                    "pty-menu",
                    &format!(
                        "state=shown tool={} reason=byte-pattern options={} signature={} signature_source={} signature_reason={} signature_chars={} jsonl_tail_bytes={}",
                        nm.tool,
                        nm.options.len(),
                        if nm.tool_call_signature.is_some() { "present" } else { "absent" },
                        nm.signature_source.unwrap_or("none"),
                        signature_trace.map(|t| t.1).unwrap_or("not-checked"),
                        nm.tool_call_signature
                            .as_ref()
                            .map(|s| s.chars().count())
                            .unwrap_or_else(|| signature_trace.map(|t| t.2).unwrap_or(0)),
                        signature_trace.map(|t| t.3).unwrap_or(0)
                    ),
                ),
                (None, Some(prev)) if prev.tool != PENDING_TOOL => append_bram_trace_line(
                    app,
                    "pty-menu",
                    &format!("state=dismissed tool={} reason=buffer-evicted", prev.tool),
                ),
                _ => {}
            }
        }

        *menu = detected;

        if should_emit_change {
            // Emit only when the user-visible menu state changes.
            // PtyMenu's PartialEq compares `tool` only — text/cursor
            // drift no longer flaps the emit cadence. Refs #77.
            emit_payload = Some(menu.clone());
            // If we're emitting a menu without a signature, schedule a
            // deferred re-check. JSONL flush typically completes within
            // sub-100ms after the assistant emits the tool_use, but it
            // can lag if the user dismisses the menu quickly OR if no
            // further PTY activity arrives to re-trigger detection.
            // Without a deferred re-check the PartialEq race-window fix
            // can't help — there's no cycle to compare against. The
            // recheck thread polls lookup_pending_tool_call twice
            // (200ms, 800ms) and emits an updated payload when the
            // signature arrives. Refs #170.
            if let Some(m) = menu.as_ref() {
                if m.tool != PENDING_TOOL && m.tool_call_signature.is_none() {
                    recheck_target = Some(m.tool.clone());
                }
            }
        }
    }

    if let Some(payload) = emit_payload {
        emit_replayable_payload(app, "pty-menu-changed", &payload);
    }

    if let Some(target_tool) = recheck_target {
        schedule_signature_recheck(app.clone(), target_tool);
    }
}

// Deferred JSONL re-check for a menu that was emitted with
// `tool_call_signature: None` because the assistant's `tool_use` line
// hadn't flushed to the Claude session JSONL yet at PTY detection time.
// Polls `lookup_pending_tool_call` twice (200 ms then 800 ms); if either
// poll returns a signature, updates the cached `PtyMenu` and emits a
// fresh `pty-menu-changed` so the inline permission panel can render
// the signature without waiting for a subsequent PTY chunk. Guards
// against firing for a different menu by capturing the original
// `target_tool` and checking it under the cache lock before mutating.
// Refs #170.
// Synchronous one-shot signature recheck driven by a JSONL watcher event.
// Pairs with `schedule_signature_recheck` (timer-based) for the case
// where the timer cadence (5.2 s cumulative) is too tight for very
// large `tool_use` payloads — Claude flushes a multi-KB issue body
// only when it's done writing, which can exceed the timer window. The
// watcher sees the JSONL grow at the exact moment of that flush; this
// helper turns that signal into an immediate retry. No-op when no
// menu is shown or the signature is already populated. Refs #170.
fn try_signature_recheck_on_jsonl_change<R: tauri::Runtime>(app: &AppHandle<R>) {
    let needs_lookup = {
        if let Ok(menu) = pty_menu_cell().lock() {
            menu.as_ref()
                .map(|m| m.tool != PENDING_TOOL && m.tool_call_signature.is_none())
                .unwrap_or(false)
        } else {
            false
        }
    };
    if !needs_lookup {
        return;
    }
    let lookup = lookup_pending_tool_call(app);
    if lookup.signature.is_none() {
        return;
    }
    let mut emit_payload: Option<Option<PtyMenu>> = None;
    let mut updated_tool: Option<String> = None;
    let mut updated_chars: usize = 0;
    if let Ok(mut menu) = pty_menu_cell().lock() {
        if let Some(m) = menu.as_mut() {
            if m.tool != PENDING_TOOL && m.tool_call_signature.is_none() {
                updated_tool = Some(m.tool.clone());
                updated_chars = lookup
                    .signature
                    .as_ref()
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
                m.tool_call_signature = lookup.signature.clone();
                m.tool_call_diff = lookup.diff.clone();
                m.signature_source = Some("jsonl");
                emit_payload = Some(menu.clone());
            }
        }
    }
    if let (Some(payload), Some(tool)) = (emit_payload, updated_tool) {
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "pty-menu",
                &format!(
                    "state=updated tool={} reason=signature-watcher-recheck signature_source=jsonl signature_chars={}",
                    tool, updated_chars
                ),
            );
        }
        emit_replayable_payload(app, "pty-menu-changed", &payload);
    }
}

fn schedule_signature_recheck<R: tauri::Runtime>(app_handle: AppHandle<R>, target_tool: String) {
    let app_for_trace = app_handle.clone();
    let target_for_trace = target_tool.clone();
    std::thread::spawn(move || {
        if bram_trace_enabled() {
            append_bram_trace_line(
                &app_for_trace,
                "pty-menu",
                &format!(
                    "state=recheck-spawn tool={} reason=signature-deferred-recheck",
                    target_for_trace
                ),
            );
        }
        // Stretched recheck schedule: original [200, 800] missed cases
        // where the JSONL hadn't flushed by 1 s (observed live at
        // 2026-06-02 ~04:35Z, user reported 10-second wait with
        // signature still absent). The new cadence covers up to ~5 s
        // wall-clock so realistic JSONL flush delays are tolerated.
        // Each tick is cumulative sleep time, not a delta. Refs #170.
        for delay_ms in [200u64, 500u64, 1500u64, 3000u64] {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            // Quick out: if the cached menu's tool no longer matches
            // (user clicked or a different menu took its place) or
            // signature was already populated by some other path,
            // there's nothing to do.
            let still_needs = {
                if let Ok(menu) = pty_menu_cell().lock() {
                    menu.as_ref()
                        .map(|m| m.tool == target_tool && m.tool_call_signature.is_none())
                        .unwrap_or(false)
                } else {
                    false
                }
            };
            if !still_needs {
                return;
            }
            let lookup = lookup_pending_tool_call(&app_handle);
            if bram_trace_enabled() {
                append_bram_trace_line(
                    &app_handle,
                    "pty-menu",
                    &format!(
                        "state=recheck-poll tool={} delay_ms={} signature={} signature_source={} reason={} signature_chars={} tail_bytes={}",
                        target_tool,
                        delay_ms,
                        if lookup.signature.is_some() { "present" } else { "absent" },
                        if lookup.signature.is_some() { "jsonl" } else { "none" },
                        lookup.reason,
                        lookup.signature.as_ref().map(|s| s.chars().count()).unwrap_or(0),
                        lookup.tail_bytes
                    ),
                );
            }
            if lookup.signature.is_none() {
                continue;
            }
            // Update under the lock, re-checking the conditions in case
            // the cached menu shifted between the unlocked lookup and
            // the locked write.
            let mut emit_payload: Option<Option<PtyMenu>> = None;
            if let Ok(mut menu) = pty_menu_cell().lock() {
                if let Some(m) = menu.as_mut() {
                    if m.tool == target_tool && m.tool_call_signature.is_none() {
                        m.tool_call_signature = lookup.signature.clone();
                        m.tool_call_diff = lookup.diff.clone();
                        m.signature_source = Some("jsonl");
                        emit_payload = Some(menu.clone());
                    }
                }
            }
            if let Some(payload) = emit_payload {
                if bram_trace_enabled() {
                    let signature_chars = payload
                        .as_ref()
                        .and_then(|p| p.tool_call_signature.as_ref())
                        .map(|s| s.chars().count())
                        .unwrap_or(0);
                    append_bram_trace_line(
                        &app_handle,
                        "pty-menu",
                        &format!(
                            "state=updated tool={} reason=signature-deferred-recheck signature_source=jsonl delay_ms={} signature_chars={}",
                            target_tool, delay_ms, signature_chars
                        ),
                    );
                }
                emit_replayable_payload(&app_handle, "pty-menu-changed", &payload);
            }
            return;
        }
    });
}

// Strip ANSI escape sequences so literal-byte matchers aren't fragmented
// by cursor-positioning / color codes that xterm.js renders correctly
// but a byte-level scan does not. Handles the forms Claude Code's TUI
// emits: CSI (ESC '[' params final-byte 0x40..=0x7E), OSC (ESC ']' ...
// terminated by BEL or ESC '\\'), and plain ESC + single byte.
fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == 0x1B && i + 1 < input.len() {
            match input[i + 1] {
                b'[' => {
                    let mut j = i + 2;
                    let mut final_byte = 0u8;
                    while j < input.len() {
                        let c = input[j];
                        j += 1;
                        if (0x40..=0x7E).contains(&c) {
                            final_byte = c;
                            break;
                        }
                    }
                    // Claude Code's TUI (Ink/React-style) positions each
                    // word in a menu/help row with a CSI "cursor column
                    // absolute" escape (`\x1b[<N>G`) instead of writing
                    // space bytes between words. Verified live: a raw-tail
                    // capture during a real Bash permission menu showed
                    // `\x1b[5G2.\x1b[8GYes,\x1b[13Gand\x1b[17Gallow…` etc.
                    // Emit one space for CSI G so the parsed option
                    // labels remain legible; other CSI codes (color `m`,
                    // erase `K/J`, cursor-movement that doesn't imply a
                    // visual gap) are still dropped entirely.
                    if final_byte == b'G' {
                        out.push(b' ');
                    }
                    i = j;
                    continue;
                }
                b']' => {
                    let mut j = i + 2;
                    while j < input.len() {
                        if input[j] == 0x07 {
                            j += 1;
                            break;
                        }
                        if input[j] == 0x1B && j + 1 < input.len() && input[j + 1] == b'\\' {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                _ => {
                    i += 2;
                    continue;
                }
            }
        }
        out.push(b);
        i += 1;
    }
    out
}

// Find the newest cursor-anchored first option: ❯ (U+276F) followed by
// an optional run of spaces / NBSP, then "1.". Claude Code's TUI once
// rendered the gap as cursor-positioning escapes (collapsing to "❯1."
// after strip_ansi); newer builds emit a literal space and/or NBSP
// (U+00A0 = c2 a0), giving "❯ 1." / "❯\u{a0} 1.". Tolerate all three so
// the anchor survives the format drift. Walk back to older arrows when
// the newest one is a redraw artifact rather than the option-1 row.
// Refs #36.
// Scan the ANSI-stripped PTY tail for the rightmost `⏺ <Tool>(...)`
// line — Claude's standard tool-call render. The cursor glyph
// (U+23FA, three UTF-8 bytes E2 8F BA) must sit at start-of-line to
// avoid matches inside prose / tool output. Walks the argument body
// paren-balanced AND quote-aware so multi-line Bash with embedded
// quoted parens (`Bash(grep "foo (bar)" file)`) parses correctly.
// Returns `Some((tool_name, signature))` where `signature` is the full
// `<Tool>(args)` slice (matching what the panel renders). Returns None
// when no anchor is found OR the paren walk hits buffer end without
// closing — partial captures are not emitted. Refs #170 follow-up.
fn extract_pty_tool_call(stripped_tail: &[u8]) -> Option<(String, String)> {
    let cursor_glyph: &[u8] = b"\xe2\x8f\xba";
    let mut search_end = stripped_tail.len();
    while let Some(rel) = stripped_tail[..search_end]
        .windows(cursor_glyph.len())
        .rposition(|w| w == cursor_glyph)
    {
        // Anchor must be the first non-whitespace on its line. Claude
        // positions the cursor at column 3 via ANSI escapes; after
        // `strip_ansi` those positioning bytes are gone but the cursor
        // glyph still sits with leading whitespace before it. Walk back
        // to the line start (or buffer start), then verify only spaces /
        // tabs / carriage returns intervene. Refs #170 follow-up.
        let line_start = stripped_tail[..rel]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prefix = &stripped_tail[line_start..rel];
        let at_line_start = prefix
            .iter()
            .all(|&b| b == b' ' || b == b'\t' || b == b'\r');
        if !at_line_start {
            search_end = rel;
            continue;
        }
        let mut k = rel + cursor_glyph.len();
        if stripped_tail.get(k) == Some(&b' ') {
            k += 1;
        }
        let name_start = k;
        while let Some(&b) = stripped_tail.get(k) {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b':' {
                k += 1;
            } else {
                break;
            }
        }
        if k == name_start || stripped_tail.get(k) != Some(&b'(') {
            search_end = rel;
            continue;
        }
        let Ok(name) = std::str::from_utf8(&stripped_tail[name_start..k]) else {
            search_end = rel;
            continue;
        };
        let body_start = k + 1;
        let mut depth: i32 = 1;
        let mut in_single = false;
        let mut in_double = false;
        let mut escape = false;
        let mut end_idx = body_start;
        let mut found_close: Option<usize> = None;
        while let Some(&b) = stripped_tail.get(end_idx) {
            if escape {
                escape = false;
            } else if (in_single || in_double) && b == b'\\' {
                escape = true;
            } else if in_double {
                if b == b'"' {
                    in_double = false;
                }
            } else if in_single {
                if b == b'\'' {
                    in_single = false;
                }
            } else if b == b'"' {
                in_double = true;
            } else if b == b'\'' {
                in_single = true;
            } else if b == b'(' {
                depth += 1;
            } else if b == b')' {
                depth -= 1;
                if depth == 0 {
                    found_close = Some(end_idx);
                    break;
                }
            }
            end_idx += 1;
        }
        if let Some(close_idx) = found_close {
            let signature_bytes = &stripped_tail[name_start..=close_idx];
            if let Ok(signature) = std::str::from_utf8(signature_bytes) {
                return Some((name.to_string(), signature.to_string()));
            }
        }
        search_end = rel;
    }
    None
}

fn pty_menu_anchor_pos(tail: &[u8]) -> Option<usize> {
    let arrow: &[u8] = b"\xe2\x9d\xaf";
    let mut end = tail.len();
    while let Some(rel) = tail[..end].windows(arrow.len()).rposition(|w| w == arrow) {
        let mut k = rel + arrow.len();
        loop {
            if tail.get(k) == Some(&0x20) {
                k += 1;
            } else if tail.get(k) == Some(&0xc2) && tail.get(k + 1) == Some(&0xa0) {
                k += 2;
            } else {
                break;
            }
        }
        if tail[k..].starts_with(b"1.") {
            return Some(rel);
        }
        end = rel;
    }
    None
}

fn pty_numbered_menu_anchor_pos(tail: &[u8]) -> Option<usize> {
    let mut end = tail.len();
    while let Some(rel) = tail[..end].windows(2).rposition(|w| w == b"1.") {
        let line_start = tail[..rel]
            .iter()
            .rposition(|&b| b == b'\n' || b == b'\r')
            .map(|idx| idx + 1)
            .unwrap_or(0);
        if tail[line_start..rel]
            .iter()
            .all(|&b| b == b' ' || b == b'\t')
        {
            return Some(rel);
        }
        end = rel;
    }
    None
}

fn pty_any_numbered_menu_anchor_pos(tail: &[u8]) -> Option<usize> {
    tail.windows(2).rposition(|w| w == b"1.")
}

fn pty_text_looks_like_permission_menu(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("do you want")
        || lower.contains("action required")
        || lower.contains("codex to run")
        || lower.contains("approve")
        || lower.contains("approval")
        || lower.contains("permission")
        || lower.contains("sandbox")
        || (lower.contains("allow") && lower.contains("deny"))
}

fn pty_text_looks_like_stale_worklist_menu_false_positive(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("diff --git")
        || lower.contains("worklist")
        || lower.contains("resources/worklist")
        || lower.contains("src-tauri/src/lib.rs")
}

fn pty_codex_action_required_pos(tail: &[u8]) -> Option<usize> {
    let title = b"Action Required";
    let mut end = tail.len();
    while let Some(pos) = tail[..end].windows(title.len()).rposition(|w| w == title) {
        let prefix_start = pos.saturating_sub(16);
        let prefix = &tail[prefix_start..pos];
        let suffix_end = (pos + title.len() + 32).min(tail.len());
        let suffix = &tail[pos + title.len()..suffix_end];
        if prefix.windows(4).any(|w| w == b"\x1b]0;") && suffix.windows(7).any(|w| w == b" | bram")
        {
            return Some(pos);
        }
        end = pos;
    }
    None
}

fn codex_fragmented_menu_options(text: &str) -> Vec<MenuOption> {
    let parsed = parse_menu_options(text);
    if !parsed.is_empty() {
        return parsed;
    }

    let mut keys: Vec<String> = Vec::new();
    for raw in text.lines() {
        let trimmed =
            raw.trim_start_matches(|c: char| c == '❯' || c == '\u{a0}' || c.is_whitespace());
        let Some((digits, rest)) = trimmed.split_once('.') else {
            continue;
        };
        if digits.is_empty()
            || !digits.chars().all(|c| c.is_ascii_digit())
            || !rest.trim().is_empty()
        {
            continue;
        }
        let key = digits.to_string();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }

    keys.into_iter()
        .map(|key| MenuOption {
            label: format!("Option {key}"),
            key,
        })
        .collect()
}

fn codex_menu_line_is_numbered_option(line: &str) -> bool {
    let trimmed = line.trim_start_matches(|c: char| c == '❯' || c == '\u{a0}' || c.is_whitespace());
    let Some((digits, _rest)) = trimmed.split_once('.') else {
        return false;
    };
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

fn codex_menu_line_is_command_candidate(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("action required")
        || lower.contains("codex to run")
        || lower.contains("approve")
        || lower.contains("approval")
        || lower.contains("permission")
        || lower.contains("sandbox")
        || lower.contains("allow")
        || lower.contains("deny")
        || lower.starts_with("esc to cancel")
        || codex_menu_line_is_numbered_option(trimmed)
    {
        return false;
    }
    true
}

fn codex_command_end(candidate: &str) -> usize {
    for (idx, ch) in candidate.char_indices() {
        if ch == '\n' || ch == '\r' || ch == '❯' || ch == '›' {
            return idx;
        }
    }
    candidate.len()
}

fn extract_codex_dollar_prompt_command(text: &str) -> Option<String> {
    for (idx, _) in text.match_indices('$').rev() {
        let candidate = text[idx + 1..].trim_start();
        let end = codex_command_end(candidate);
        let command = candidate[..end].trim();
        if !command.is_empty() {
            return Some(command.to_string());
        }
    }
    None
}

fn codex_prompt_field_value(text: &str, marker: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let idx = lower.find(marker)?;
    let mut value = String::new();
    let mut started = false;
    for ch in text[idx + marker.len()..].chars() {
        if !started {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '/' || ch == '.' {
                started = true;
                value.push(ch);
            }
            continue;
        }
        if ch == '"'
            || ch == '\''
            || ch == '\\'
            || ch == ','
            || ch == '}'
            || ch == '>'
            || ch == '›'
            || ch == '❯'
            || ch == '\n'
            || ch == '\r'
            || ch.is_whitespace()
        {
            break;
        }
        value.push(ch);
    }
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn codex_path_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            '/' | '.' | '-' | '_' | '+' | '~' | '@' | ':' | '=' | '%'
        )
}

fn extract_longest_absolute_path(text: &str) -> Option<String> {
    let mut best = String::new();
    for (idx, _) in text.match_indices('/') {
        let mut candidate = String::new();
        for ch in text[idx..].chars() {
            if codex_path_char(ch) {
                candidate.push(ch);
            } else {
                break;
            }
        }
        if candidate.len() > best.len() {
            best = candidate;
        }
    }
    if best.is_empty() {
        None
    } else {
        Some(best)
    }
}

fn extract_codex_mcp_signature(text: &str) -> Option<(String, String)> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("mcp") {
        return None;
    }
    let tool = codex_prompt_field_value(text, "tool")?;
    let path =
        extract_longest_absolute_path(text).or_else(|| codex_prompt_field_value(text, "path:"))?;
    Some((tool.clone(), format!("filesystem:{}({})", tool, path)))
}

fn extract_codex_command_signature(text: &str) -> Option<(String, String)> {
    if let Some(sig) = extract_codex_mcp_signature(text) {
        return Some(sig);
    }
    if let Some(command) = extract_codex_dollar_prompt_command(text) {
        return Some(("Bash".to_string(), format!("Bash({})", command)));
    }

    let mut candidates: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if codex_menu_line_is_numbered_option(line) {
            break;
        }
        if codex_menu_line_is_command_candidate(line) {
            candidates.push(line.trim_start_matches("$ ").to_string());
        }
    }
    let command = candidates.last()?.trim();
    if command.is_empty() {
        None
    } else {
        Some(("Bash".to_string(), format!("Bash({})", command)))
    }
}

// Look for claude's permission menu in the rolling tail. Pattern:
// "1. Yes" appears, followed by "2. " within ~512 bytes (the menu's
// options are tightly grouped). Tool name is best-effort guessed
// from preceding context. Runs on the ANSI-stripped tail — the raw
// bytes contain escape sequences interleaved within the visible menu
// text, which would fragment the literal needle match.
fn pty_menu_detect(tail: &[u8]) -> Option<PtyMenu> {
    let raw_codex_action = pty_codex_action_required_pos(tail).is_some();
    let stripped = strip_ansi(tail);
    let tail = stripped.as_slice();
    // Prefer Claude's selection-cursor anchor (❯, U+276F) followed by an
    // optional run of spaces / NBSP, then "1.". Codex prompts can render
    // as a plain numbered approval menu, but only trust that shape when
    // the raw PTY title says Action Required. Plain numbered prose in the
    // transcript/worklist stream is otherwise too easy to mistake for a
    // permission menu.
    let needle2: &[u8] = b"2.";
    let header: &[u8] = b"Do you want";
    let pos1_from_cursor = pty_menu_anchor_pos(tail);
    let pos1_opt = pos1_from_cursor.or_else(|| {
        if raw_codex_action {
            pty_numbered_menu_anchor_pos(tail).or_else(|| pty_any_numbered_menu_anchor_pos(tail))
        } else {
            None
        }
    });
    let pos_header = tail.windows(header.len()).rposition(|w| w == header);

    let result = (|| -> Option<PtyMenu> {
        let pos1 = pos1_opt?;
        let after = &tail[pos1..];
        let rel = after.windows(needle2.len()).position(|w| w == needle2)?;
        let pos2 = pos1 + rel;
        if pos2 - pos1 > 512 {
            return None;
        }
        let start = pos1.saturating_sub(200);
        let end = (pos2 + 200).min(tail.len());
        let text = String::from_utf8_lossy(&tail[start..end]).into_owned();
        if pos1_from_cursor.is_none()
            && !raw_codex_action
            && !pty_text_looks_like_permission_menu(&text)
        {
            return None;
        }
        if pos1_from_cursor.is_none()
            && !raw_codex_action
            && pty_text_looks_like_stale_worklist_menu_false_positive(&text)
        {
            return None;
        }
        // Prefer parsing the tool name from the menu's own
        // "Do you want to use X?" header (which lives inside `text`).
        // Falls back to the pre-menu context grep when the header is
        // missing or unparseable. The header-parse moves with the menu
        // through the rolling buffer, so the reported tool name stays
        // stable across short eviction cycles instead of flipping to
        // whatever earlier prompt's tool name happens to still be in
        // the 200-byte pre-context window. Refs #77 menu-detector
        // stabilization (the 18:52:51Z 31-events-in-one-second burst
        // with Bash <-> Tool <-> Read flapping).
        let tool = match pty_menu_tool_from_header(&text) {
            Some(t) => t,
            None => {
                if raw_codex_action {
                    let options = codex_fragmented_menu_options(&text);
                    let extracted = extract_codex_command_signature(&text);
                    let tool = extracted
                        .as_ref()
                        .map(|(tool, _)| tool.clone())
                        .unwrap_or_else(|| "Bash".to_string());
                    let signature = extracted.map(|(_, signature)| signature);
                    let signature_source = if signature.is_some() {
                        Some("pty")
                    } else {
                        None
                    };
                    return Some(PtyMenu {
                        tool,
                        text,
                        options,
                        tool_call_signature: signature,
                        tool_call_diff: None,
                        signature_source,
                    });
                }
                // Header text hasn't landed in this cycle's tail. If
                // the previous cycle was already pending, we've waited
                // a full cycle and the header still isn't here — fall
                // back to the pre-menu grep now. Otherwise mark the
                // menu pending so `pty_menu_update` suppresses the
                // shown emit until we either get a header next cycle
                // or convert to grep on the cycle after. Refs #77
                // tighten-pty-menu-emit-cadence.
                let prev_was_pending = pty_menu_cell()
                    .lock()
                    .ok()
                    .and_then(|m| m.as_ref().map(|p| p.tool == PENDING_TOOL))
                    .unwrap_or(false);
                if prev_was_pending {
                    pty_menu_guess_tool(&tail[..pos1])
                } else {
                    PENDING_TOOL.to_string()
                }
            }
        };
        let options = if raw_codex_action {
            codex_fragmented_menu_options(&text)
        } else {
            parse_menu_options(&text)
        };
        // PTY-preferred signature. Scan the full ANSI-stripped tail for
        // `⏺ <Tool>(...)`. When found, override the menu's tool name with
        // the PTY-derived one — `pty_menu_tool_from_header` /
        // `pty_menu_guess_tool` have proven unreliable (live: "MultiEdit"
        // header for a Bash gh issue comment, and again for a Write). The
        // PTY scan finds the actual tool call. Refs #170 follow-up.
        let pty_call = extract_pty_tool_call(tail);
        let codex_signature = if raw_codex_action {
            extract_codex_command_signature(&text)
        } else {
            None
        };
        let (final_tool, signature, signature_source) = match pty_call {
            Some((name, signature)) => (name, Some(signature), Some("pty")),
            None if codex_signature.is_some() => {
                let (tool, signature) = codex_signature.unwrap();
                (tool, Some(signature), Some("pty"))
            }
            None => (tool, None, None),
        };
        Some(PtyMenu {
            tool: final_tool,
            text,
            options,
            tool_call_signature: signature,
            tool_call_diff: None,
            signature_source,
        })
    })();

    // Diagnostic: when the menu prompt header is present but detection
    // returned None, log what we found AND dump the full stripped tail
    // to /tmp/pty-menu-snapshot.txt so we can iterate on the matcher.
    if result.is_none() {
        if let Some(h) = pos_header {
            let pos2_after_pos1 = pos1_opt.and_then(|p1| {
                tail[p1..]
                    .windows(needle2.len())
                    .position(|w| w == needle2)
                    .map(|rel| p1 + rel)
            });
            let dump_end = (h + 300).min(tail.len());
            let dump = &tail[h..dump_end];
            let mut printable = String::new();
            for &b in dump {
                match b {
                    b'\n' => printable.push_str("\\n"),
                    b'\r' => printable.push_str("\\r"),
                    b'\t' => printable.push_str("\\t"),
                    0x20..=0x7E => printable.push(b as char),
                    _ => printable.push_str(&format!("\\x{:02x}", b)),
                }
            }
            eprintln!(
                "[pty-menu-trace] miss: stripped_len={} header_at={} '1. Yes'_at={:?} '2. '_after_pos1_at={:?} after_header={:?}",
                tail.len(),
                h,
                pos1_opt,
                pos2_after_pos1,
                printable
            );
            let _ = std::fs::write("/tmp/pty-menu-snapshot.txt", tail);
        }
    }

    result
}

// Extract the tool name from a "Do you want to use X?" prompt header
// inside the captured menu text. Returns None when the header is absent
// or unparseable so the caller can fall back to pre-menu context grep.
// The token after "use " is read up to the first non-name character;
// covers `Bash`, `Edit`, `Write`, `MultiEdit`, `Read`, `mcp__foo__bar`,
// `WebFetch`, etc. Trailing punctuation (`?`, `,`, whitespace) is not
// captured. Refs #77 menu-detector stabilization.
fn pty_menu_tool_from_header(text: &str) -> Option<String> {
    let needle = "Do you want to use ";
    let start = text.find(needle)? + needle.len();
    let rest = &text[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.' && c != '-')
        .unwrap_or(rest.len());
    let tok = &rest[..end];
    if tok.is_empty() {
        None
    } else {
        Some(tok.to_string())
    }
}

fn pty_menu_guess_tool(context: &[u8]) -> String {
    let s = String::from_utf8_lossy(context);
    for tool in &[
        "MultiEdit",
        "ToolSearch",
        "WebFetch",
        "WebSearch",
        "Bash",
        "Edit",
        "Write",
        "Read",
        "Grep",
        "Glob",
    ] {
        if s.contains(tool) {
            return (*tool).to_string();
        }
    }
    "Tool".to_string()
}

fn pty_menu_input_clears_inflight(input: &str) -> bool {
    input == "\x1b" || input == "3\r" || input == "3\n"
}

fn pty_output_clears_inflight(output: &[u8]) -> bool {
    let stripped = strip_ansi(output);
    let text = String::from_utf8_lossy(&stripped);
    text.contains("You canceled the request")
        || text.contains("You cancelled the request")
        || text.contains("Conversation interrupted")
}

// Called from pty_write on user input. Records the dismissed menu's
// tool name into PTY_MENU_SUPPRESSED so the detector won't immediately
// re-fire when the next PTY chunk arrives (the dismissed text is still
// in the rolling buffer).
fn pty_menu_clear<R: tauri::Runtime>(app: &AppHandle<R>, input: &str) {
    let dismissed_tool: Option<String> = match pty_menu_cell().lock() {
        Ok(mut menu) => {
            let tool = menu.as_ref().map(|m| m.tool.clone());
            *menu = None;
            tool
        }
        Err(_) => None,
    };
    // Drain PTY_TAIL so the dismissed menu's bytes can't trigger a stale
    // re-detection once PTY_MENU_SUPPRESSED expires. Only genuinely new
    // PTY output can re-fire the detector after this point.
    if let Ok(mut tail) = pty_tail_cell().lock() {
        tail.clear();
    }
    // User dismissal supersedes any pending eviction-grace deferral.
    if let Ok(mut grace) = pty_menu_eviction_grace_cell().lock() {
        *grace = None;
    }
    if let Some(tool) = dismissed_tool {
        let clears_inflight = pty_menu_input_clears_inflight(input);
        // Pending menus never emitted `state=shown` to subscribers
        // (their tool name hadn't resolved yet). Don't emit the matching
        // `state=dismissed` trace and don't add a re-detection
        // suppression entry — there's no concrete tool name to suppress
        // against, and the iframe was never told about the menu so it
        // has nothing to clear. Refs #77 tighten-pty-menu-emit-cadence.
        if tool == PENDING_TOOL {
            eprintln!("[pty-menu] cleared by user input (pending menu — shown emit was deferred)");
            if clears_inflight {
                clear_active_sentinel_with_reason(app, "pty-menu-pending-user-reject");
            }
            return;
        }
        eprintln!(
            "[pty-menu] cleared by user input (tool={}, suppressing for 2s)",
            tool
        );
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "pty-menu",
                &format!("state=dismissed tool={} reason=user-input", tool),
            );
        }
        if let Ok(mut s) = pty_menu_suppressed_cell().lock() {
            *s = Some((tool, std::time::Instant::now()));
        }
        if clears_inflight {
            clear_active_sentinel_with_reason(app, "pty-menu-user-reject");
        }
        // Tell subscribers the menu went away. Emit AFTER releasing all
        // pty_menu_* locks for the same anti-deadlock reason as in
        // pty_menu_update.
        emit_replayable_payload(app, "pty-menu-changed", Option::<PtyMenu>::None);
    } else if input == "\x1b" {
        clear_active_sentinel_with_reason(app, "pty-escape");
        // Single Esc usually just interrupts the current tool call; CC
        // continues the same turn. But a rapid SECOND Esc (within ~1 s)
        // is CC's universal "cancel everything" gesture — treat that as
        // an explicit user-cancel and hard-kill the row.
        let now = unix_now_ms();
        let prev = last_pty_escape_ms_cell().lock().map(|g| *g).unwrap_or(0);
        if let Ok(mut g) = last_pty_escape_ms_cell().lock() {
            *g = now;
        }
        const ESC_DOUBLE_TAP_WINDOW_MS: i64 = 1000;
        if now - prev < ESC_DOUBLE_TAP_WINDOW_MS && prev > 0 {
            kill_current_claude_turn(app);
        }
    }
}

// Hex+ASCII dump of bytes — for the /__pty-tail debug endpoint.
fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4);
    for (i, chunk) in bytes.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", i * 16));
        for (j, b) in chunk.iter().enumerate() {
            out.push_str(&format!("{:02x} ", b));
            if j == 7 {
                out.push(' ');
            }
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        if chunk.len() <= 8 {
            out.push(' ');
        }
        out.push_str(" |");
        for b in chunk {
            if (0x20..0x7f).contains(b) {
                out.push(*b as char);
            } else {
                out.push('.');
            }
        }
        out.push_str("|\n");
    }
    out
}

fn compare_versions(current: &str, latest: &str) -> bool {
    let parse =
        |s: &str| -> Vec<u32> { s.split('.').filter_map(|x| x.parse::<u32>().ok()).collect() };
    parse(latest) > parse(current)
}

fn fetch_app_info() -> AppInfo {
    let current = first_nonempty_env(&["BRAM_FAKE_CURRENT", "XMLUI_DESKTOP_FAKE_CURRENT"])
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    // curl ships on macOS / Linux / Windows 10+; avoids pulling in an HTTP
    // dependency for a single, tolerant-of-failure fetch.
    let output = std::process::Command::new("curl")
        .args([
            "-sf",
            "-m",
            "5",
            "-H",
            "User-Agent: bram",
            "-H",
            "Accept: application/vnd.github+json",
            "https://api.github.com/repos/judell/bram/releases/latest",
        ])
        .output();

    let bytes = match output {
        Ok(o) if o.status.success() => o.stdout,
        _ => {
            return AppInfo {
                current,
                latest: None,
                has_update: false,
                release_url: None,
            }
        }
    };

    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return AppInfo {
                current,
                latest: None,
                has_update: false,
                release_url: None,
            }
        }
    };

    let tag = v.get("tag_name").and_then(|x| x.as_str()).unwrap_or("");
    let latest_str = tag.trim_start_matches('v').to_string();
    if latest_str.is_empty() {
        return AppInfo {
            current,
            latest: None,
            has_update: false,
            release_url: None,
        };
    }
    let release_url = v.get("html_url").and_then(|x| x.as_str()).map(String::from);
    let has_update = compare_versions(&current, &latest_str);
    AppInfo {
        current,
        latest: Some(latest_str),
        has_update,
        release_url,
    }
}

fn get_app_info() -> AppInfo {
    let cache = APP_INFO_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap();
    if let Some(cached) = guard.as_ref() {
        return cached.clone();
    }
    let info = fetch_app_info();
    *guard = Some(info.clone());
    info
}

fn git_log_recent<R: tauri::Runtime>(app: &AppHandle<R>, count: usize) -> Result<Vec<u8>, String> {
    use std::collections::HashSet;
    // Determine which commits are ahead of the remote; treat the rest as pushed.
    // If there's no upstream tracking, we just call everything "unpushed".
    let unpushed: HashSet<String> = git_run(app, &["rev-list", "@{u}..HEAD"])
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    // Resolve the GitHub URL for the html_url field, if any.
    let remote_url = git_run(app, &["remote", "get-url", "origin"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let html_base = remote_to_html(&remote_url);

    // Local git identity + GitHub login so we can map commits authored by
    // the current user to their actual GH username (not just their email
    // local-part — `jon@jonudell.info` resolves to GH login `judell`).
    let local_email = git_run(app, &["config", "user.email"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let local_login: Option<String> = project_root(Some(app)).and_then(|root| {
        std::process::Command::new("gh")
            .current_dir(&root)
            .args(&["api", "/user", "--jq", ".login"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    });

    let count_arg = format!("-n{}", count);
    // __C__ sentinel marks the start of each commit; --shortstat lines
    // appear in between. Merge commits and commits with no file changes
    // emit no shortstat line, so we finalize on the next sentinel.
    // %ae = full author email (matched against local git user.email);
    // %al = email local-part used as a fallback / for non-local authors.
    let format = "--format=__C__%H%x09%an%x09%aI%x09%ae%x09%al%x09%s";
    let log_out = git_run(app, &["log", &count_arg, "--shortstat", format])?;

    let mut commits: Vec<serde_json::Value> = Vec::new();
    let mut header_parts: Option<(String, String, String, String, String)> = None;
    let mut additions: u64 = 0;
    let mut deletions: u64 = 0;

    let finalize = |hdr: &Option<(String, String, String, String, String)>,
                    adds: u64,
                    dels: u64,
                    out: &mut Vec<serde_json::Value>| {
        if let Some((sha, author, date, login, subject)) = hdr {
            let pushed = !unpushed.contains(sha);
            let html_url = if html_base.is_empty() {
                String::new()
            } else {
                format!("{}/commit/{}", html_base, sha)
            };
            out.push(serde_json::json!({
                "sha": sha,
                "html_url": html_url,
                "pushed": pushed,
                "additions": adds,
                "deletions": dels,
                "commit": {
                    "author": { "name": author, "date": date, "login": login },
                    "message": subject,
                },
            }));
        }
    };

    for line in log_out.lines() {
        if let Some(rest) = line.strip_prefix("__C__") {
            finalize(&header_parts, additions, deletions, &mut commits);
            additions = 0;
            deletions = 0;
            let parts: Vec<&str> = rest.splitn(6, '\t').collect();
            if parts.len() == 6 {
                let author_email = parts[3];
                let raw_local = parts[4];
                // Prefer the live `gh api /user` login when the commit was
                // authored with the local git identity; otherwise strip the
                // GitHub noreply `<digits>+` prefix from the email local-part.
                let login = if !author_email.is_empty()
                    && author_email == local_email
                    && local_login.is_some()
                {
                    local_login.clone().unwrap()
                } else if let Some(idx) = raw_local.find('+') {
                    if !raw_local[..idx].is_empty()
                        && raw_local[..idx].chars().all(|c| c.is_ascii_digit())
                    {
                        raw_local[idx + 1..].to_string()
                    } else {
                        raw_local.to_string()
                    }
                } else {
                    raw_local.to_string()
                };
                header_parts = Some((
                    parts[0].to_string(),
                    parts[1].to_string(),
                    parts[2].to_string(),
                    login,
                    parts[5].to_string(),
                ));
            } else {
                header_parts = None;
            }
        } else if header_parts.is_some() {
            // Shortstat line, e.g.: " 3 files changed, 18 insertions(+), 2 deletions(-)"
            for part in line.trim().split(", ") {
                if let Some(idx) = part.find(' ') {
                    let n: u64 = part[..idx].parse().unwrap_or(0);
                    let rest = &part[idx + 1..];
                    if rest.contains("insertion") {
                        additions = n;
                    } else if rest.contains("deletion") {
                        deletions = n;
                    }
                }
            }
        }
    }
    finalize(&header_parts, additions, deletions, &mut commits);

    serde_json::to_vec(&commits).map_err(|e| e.to_string())
}

// Full-history commit search. Walks `git log` (full body via %B) and
// matches each commit's subject+body lines and author against the
// query (case-insensitive substring). Returns the Context-shaped
// payload: `{ results: [{...commit fields, hits: [{line, snippet,
// field}]}], truncated }`. Capped at MAX_RESULTS commits scanned and
// MAX_HITS total hits so a wide-net query doesn't pin git.
fn git_log_search<R: tauri::Runtime>(app: &AppHandle<R>, query: &str) -> Result<Vec<u8>, String> {
    use serde_json::json;
    use std::collections::HashSet;
    let q = query.trim();
    if q.is_empty() {
        return Ok(b"{\"results\":[],\"truncated\":false}".to_vec());
    }
    let needle = q.to_lowercase();
    const MAX_RESULTS: usize = 50;
    const MAX_HITS: usize = 200;

    let unpushed: HashSet<String> = git_run(app, &["rev-list", "@{u}..HEAD"])
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let remote_url = git_run(app, &["remote", "get-url", "origin"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let html_base = remote_to_html(&remote_url);

    // Use record/field separators that won't appear in commit
    // messages so we can reassemble multi-line bodies safely.
    // %x1e between records, %x1f between fields, body last.
    let format = "--format=%H%x1f%an%x1f%aI%x1f%B%x1e";
    let log_out = git_run(app, &["log", "-n2000", format])?;

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut total_hits = 0usize;
    let mut truncated = false;

    for record in log_out.split('\x1e') {
        let record = record.trim_start_matches('\n');
        if record.is_empty() {
            continue;
        }
        let parts: Vec<&str> = record.splitn(4, '\x1f').collect();
        if parts.len() != 4 {
            continue;
        }
        if results.len() >= MAX_RESULTS || total_hits >= MAX_HITS {
            truncated = true;
            break;
        }
        let sha = parts[0].to_string();
        let author = parts[1];
        let date = parts[2];
        let body = parts[3].trim_end_matches('\n');
        let subject = body.lines().next().unwrap_or("").to_string();

        let mut hits: Vec<serde_json::Value> = Vec::new();
        for (i, line) in body.lines().enumerate() {
            if total_hits >= MAX_HITS {
                truncated = true;
                break;
            }
            if line.to_lowercase().contains(&needle) {
                let snippet: String = line.trim().chars().take(200).collect();
                hits.push(json!({
                    "line": i + 1,
                    "snippet": snippet,
                    "field": if i == 0 { "subject" } else { "body" },
                }));
                total_hits += 1;
            }
        }
        if author.to_lowercase().contains(&needle) && total_hits < MAX_HITS {
            hits.push(json!({
                "line": 0,
                "snippet": author,
                "field": "author",
            }));
            total_hits += 1;
        }

        if hits.is_empty() {
            continue;
        }
        let pushed = !unpushed.contains(&sha);
        let html_url = if html_base.is_empty() {
            String::new()
        } else {
            format!("{}/commit/{}", html_base, sha)
        };
        results.push(json!({
            "sha": sha,
            "html_url": html_url,
            "pushed": pushed,
            "commit": {
                "author": { "name": author, "date": date },
                "message": subject,
            },
            "body": body,
            "hits": hits,
        }));
    }

    serde_json::to_vec(&json!({ "results": results, "truncated": truncated }))
        .map_err(|e| e.to_string())
}

// Shell out to `gh` to list issues for the current repo. Returns the raw
// JSON bytes from `gh`. On any failure (gh missing, not a GitHub repo,
// auth missing, etc) returns an empty JSON array so the frontend renders
// a friendly empty state rather than a 500.
// gh issue list caps at the N newest issues across all states; set well
// above the repo's total issue count so no open issue is dropped (issue
// #104). Both gh_issues_list and gh_issues_search must share this so they
// can't drift. Bump if the repo ever approaches this many issues.
const GH_ISSUE_LIST_LIMIT: &str = "500";

fn gh_issues_list<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<Vec<u8>, String> {
    let root = project_root(Some(app)).ok_or_else(|| "no project root".to_string())?;
    let repo_slug = repo_owner_name(app);
    let out = std::process::Command::new("gh")
        .current_dir(&root)
        .args(&[
            "issue",
            "list",
            "--json",
            "number,title,state,author,createdAt,updatedAt,labels,url,comments",
            "--limit",
            GH_ISSUE_LIST_LIMIT,
            "--state",
            "all",
        ])
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let mut issues: Vec<serde_json::Value> = match serde_json::from_slice(&out.stdout) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[gh issue list] parse: {}", e);
                    return Ok(b"[]".to_vec());
                }
            };
            for issue in &mut issues {
                enrich_issue_activity(app, issue, repo_slug.as_deref());
            }
            serde_json::to_vec(&issues).map_err(|e| e.to_string())
        }
        Ok(out) => {
            eprintln!(
                "[gh issue list] non-zero exit: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Ok(b"[]".to_vec())
        }
        Err(e) => {
            eprintln!("[gh issue list] failed to spawn: {}", e);
            Ok(b"[]".to_vec())
        }
    }
}

// Issue search: shells out to `gh issue list --search "<q>"`, then for each
// matched issue computes per-line hits across `title` and `body` (no comment
// search yet — adding that requires a second `gh issue view` per hit). Same
// shape as Commits search: { results: [{...fields, hits: [{line, snippet,
// field}]}], truncated }. On any gh failure returns the empty envelope so
// the frontend renders cleanly.
fn gh_issues_search<R: tauri::Runtime>(app: &AppHandle<R>, query: &str) -> Result<Vec<u8>, String> {
    use serde_json::json;
    let q = query.trim();
    if q.is_empty() {
        return Ok(b"{\"results\":[],\"truncated\":false}".to_vec());
    }
    let needle = q.to_lowercase();
    const MAX_HITS: usize = 200;

    let root = project_root(Some(app)).ok_or_else(|| "no project root".to_string())?;
    let repo_slug = repo_owner_name(app);
    // Fetch the same issue window as gh_issues_list (shared
    // GH_ISSUE_LIST_LIMIT, no --search flag); local grep over title + body
    // + comment bodies. One gh call; latency scales with the actual issue
    // count, gaining comment-text search for free without doubling it.
    let out = std::process::Command::new("gh")
        .current_dir(&root)
        .args(&[
            "issue",
            "list",
            "--json",
            "number,title,state,author,createdAt,updatedAt,labels,url,body,comments",
            "--limit",
            GH_ISSUE_LIST_LIMIT,
            "--state",
            "all",
        ])
        .output();
    let stdout = match out {
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            eprintln!(
                "[gh issue list] non-zero exit: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return Ok(b"{\"results\":[],\"truncated\":false}".to_vec());
        }
        Err(e) => {
            eprintln!("[gh issue list] failed to spawn: {}", e);
            return Ok(b"{\"results\":[],\"truncated\":false}".to_vec());
        }
    };
    let issues: Vec<serde_json::Value> = match serde_json::from_slice(&stdout) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[gh issue list] parse: {}", e);
            return Ok(b"{\"results\":[],\"truncated\":false}".to_vec());
        }
    };

    let mut results: Vec<(usize, serde_json::Value)> = Vec::new();
    let mut total_hits = 0usize;
    let mut truncated = false;

    for (issue_index, mut issue) in issues.into_iter().enumerate() {
        enrich_issue_activity(app, &mut issue, repo_slug.as_deref());
        if total_hits >= MAX_HITS {
            truncated = true;
            break;
        }
        let title = issue.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let body = issue.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let issue_author = issue
            .get("author")
            .and_then(|v| v.get("login"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let mut hits: Vec<serde_json::Value> = Vec::new();
        if title.to_lowercase().contains(&needle) {
            let snippet: String = title.trim().chars().take(200).collect();
            hits.push(json!({
                "line": 0,
                "snippet": snippet,
                "field": "title",
            }));
            total_hits += 1;
        }
        if total_hits < MAX_HITS && issue_author.to_lowercase().contains(&needle) {
            hits.push(json!({
                "line": 0,
                "snippet": issue_author,
                "field": "author",
            }));
            total_hits += 1;
        }
        for (i, line) in body.lines().enumerate() {
            if total_hits >= MAX_HITS {
                truncated = true;
                break;
            }
            if line.to_lowercase().contains(&needle) {
                let snippet: String = line.trim().chars().take(200).collect();
                hits.push(json!({
                    "line": i + 1,
                    "snippet": snippet,
                    "field": "body",
                }));
                total_hits += 1;
            }
        }
        // Grep each comment's body, per-line. `comments` is an array of
        // {body, author: {login}, ...} from the JSON fields list.
        if total_hits < MAX_HITS {
            if let Some(comments) = issue.get("comments").and_then(|v| v.as_array()) {
                'comments: for (ci, comment) in comments.iter().enumerate() {
                    let cbody = comment.get("body").and_then(|v| v.as_str()).unwrap_or("");
                    let cauthor = comment
                        .get("author")
                        .and_then(|v| v.get("login"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if cauthor.to_lowercase().contains(&needle) {
                        if total_hits >= MAX_HITS {
                            truncated = true;
                            break 'comments;
                        }
                        hits.push(json!({
                            "line": 0,
                            "snippet": cauthor,
                            "field": "author",
                            "commentIndex": ci,
                            "commentAuthor": cauthor,
                        }));
                        total_hits += 1;
                    }
                    for (i, line) in cbody.lines().enumerate() {
                        if total_hits >= MAX_HITS {
                            truncated = true;
                            break 'comments;
                        }
                        if line.to_lowercase().contains(&needle) {
                            let snippet: String = line.trim().chars().take(200).collect();
                            hits.push(json!({
                                "line": i + 1,
                                "snippet": snippet,
                                "field": "comment",
                                "commentIndex": ci,
                                "commentAuthor": cauthor,
                            }));
                            total_hits += 1;
                        }
                    }
                }
            }
        }
        if hits.is_empty() {
            continue;
        }
        let mut out_issue = issue.clone();
        if let Some(obj) = out_issue.as_object_mut() {
            obj.insert("hits".into(), serde_json::Value::Array(hits));
        }
        results.push((issue_index, out_issue));
    }
    sort_issue_search_results_by_title_hits(&mut results);
    let results: Vec<serde_json::Value> = results.into_iter().map(|(_, issue)| issue).collect();

    serde_json::to_vec(&json!({ "results": results, "truncated": truncated }))
        .map_err(|e| e.to_string())
}

fn issue_search_result_rank(issue: &serde_json::Value) -> usize {
    let has_title_hit = issue
        .get("hits")
        .and_then(|v| v.as_array())
        .map(|hits| {
            hits.iter()
                .any(|hit| hit.get("field").and_then(|field| field.as_str()) == Some("title"))
        })
        .unwrap_or(false);

    if has_title_hit {
        0
    } else {
        1
    }
}

fn sort_issue_search_results_by_title_hits(results: &mut Vec<(usize, serde_json::Value)>) {
    results.sort_by_key(|(issue_index, issue)| (issue_search_result_rank(issue), *issue_index));
}

#[cfg(test)]
mod issue_search_tests {
    use super::sort_issue_search_results_by_title_hits;
    use serde_json::{json, Value};

    fn issue(number: u64, field: &str) -> Value {
        json!({
            "number": number,
            "hits": [
                {
                    "field": field,
                    "line": 0,
                    "snippet": field
                }
            ]
        })
    }

    fn numbers(results: &[(usize, Value)]) -> Vec<u64> {
        results
            .iter()
            .map(|(_, issue)| issue.get("number").and_then(|v| v.as_u64()).unwrap())
            .collect()
    }

    #[test]
    fn title_hits_sort_before_body_and_comment_hits() {
        let mut results = vec![
            (0, issue(1, "body")),
            (1, issue(2, "comment")),
            (2, issue(3, "title")),
        ];

        sort_issue_search_results_by_title_hits(&mut results);

        assert_eq!(numbers(&results), vec![3, 1, 2]);
    }

    #[test]
    fn issue_search_rank_preserves_order_inside_each_tier() {
        let mut results = vec![
            (0, issue(1, "comment")),
            (1, issue(2, "title")),
            (2, issue(3, "body")),
            (3, issue(4, "title")),
        ];

        sort_issue_search_results_by_title_hits(&mut results);

        assert_eq!(numbers(&results), vec![2, 4, 1, 3]);
    }
}

// Shell out to `gh issue view <number> --json ...` and return the raw JSON
// bytes. Same failure envelope as gh_issues_list — empty object on any
// error so the frontend can render something rather than 500.
fn gh_issue_view<R: tauri::Runtime>(app: &AppHandle<R>, number: u64) -> Result<Vec<u8>, String> {
    let root = project_root(Some(app)).ok_or_else(|| "no project root".to_string())?;
    let n = number.to_string();
    let out = std::process::Command::new("gh")
        .current_dir(&root)
        .args(&[
            "issue",
            "view",
            &n,
            "--json",
            "number,title,body,state,author,createdAt,updatedAt,labels,url,comments",
        ])
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let mut issue: serde_json::Value = match serde_json::from_slice(&out.stdout) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[gh issue view {}] parse: {}", n, e);
                    return Ok(b"{}".to_vec());
                }
            };
            enrich_issue_activity(app, &mut issue, None);
            let cross_refs = gh_issue_cross_references(app, number);
            let is_closed = issue.get("state").and_then(|v| v.as_str()) == Some("CLOSED");
            let closed_event = if is_closed {
                repo_owner_name(app)
                    .and_then(|slug| gh_issue_closed_event_actor(app, &slug, number))
            } else {
                None
            };
            if let Some(obj) = issue.as_object_mut() {
                obj.insert(
                    "crossReferences".to_string(),
                    serde_json::Value::Array(cross_refs),
                );
                if let Some((by, by_at)) = closed_event {
                    obj.insert("closedBy".to_string(), serde_json::Value::String(by));
                    obj.insert("closedByAt".to_string(), serde_json::Value::String(by_at));
                }
            }
            serde_json::to_vec(&issue).map_err(|e| e.to_string())
        }
        Ok(out) => {
            eprintln!(
                "[gh issue view {}] non-zero exit: {}",
                n,
                String::from_utf8_lossy(&out.stderr)
            );
            Ok(b"{}".to_vec())
        }
        Err(e) => {
            eprintln!("[gh issue view {}] failed to spawn: {}", n, e);
            Ok(b"{}".to_vec())
        }
    }
}

// Fetch issues that cross-reference the given issue, via the GitHub timeline
// API. Returns [{number, title, state}, ...] sorted by issue number. State is
// normalized to uppercase to match the rest of the issue payload. Empty on any
// failure — the cross-reference list is an enhancement, not a hard
// requirement, so quiet degradation is the right behavior.
fn gh_issue_cross_references<R: tauri::Runtime>(
    app: &AppHandle<R>,
    number: u64,
) -> Vec<serde_json::Value> {
    let Some(root) = project_root(Some(app)) else {
        return vec![];
    };
    let n = number.to_string();
    let endpoint = format!("repos/:owner/:repo/issues/{}/timeline", n);
    let out = std::process::Command::new("gh")
        .current_dir(&root)
        .args(&[
            "api",
            &endpoint,
            "--jq",
            r#"[.[] | select(.event == "cross-referenced" and .source.issue) | {number: .source.issue.number, title: .source.issue.title, state: (.source.issue.state | ascii_upcase)}]"#,
        ])
        .output();
    match out {
        Ok(out) if out.status.success() => {
            serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout).unwrap_or_default()
        }
        Ok(out) => {
            eprintln!(
                "[gh api .../issues/{}/timeline] non-zero exit: {}",
                n,
                String::from_utf8_lossy(&out.stderr)
            );
            vec![]
        }
        Err(e) => {
            eprintln!("[gh api .../issues/{}/timeline] failed to spawn: {}", n, e);
            vec![]
        }
    }
}

// Post a comment to a GitHub issue via `gh issue comment <n> --body "..."`.
// Returns `{"ok":true}` on success; on failure returns the gh stderr as the
// error body so the frontend can surface it. Empty/whitespace bodies are
// rejected up front since gh would reject them anyway.
fn gh_issue_comment<R: tauri::Runtime>(
    app: &AppHandle<R>,
    number: u64,
    body: &str,
) -> Result<Vec<u8>, String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err("empty comment body".to_string());
    }
    let root = project_root(Some(app)).ok_or_else(|| "no project root".to_string())?;
    let n = number.to_string();
    let out = std::process::Command::new("gh")
        .current_dir(&root)
        .args(&["issue", "comment", &n, "--body", trimmed])
        .output()
        .map_err(|e| format!("failed to spawn gh: {}", e))?;
    if out.status.success() {
        Ok(b"{\"ok\":true}".to_vec())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        eprintln!("[gh issue comment {}] non-zero exit: {}", n, stderr);
        Err(stderr)
    }
}

// Close a GitHub issue via `gh issue close <n>`, optionally with a comment.
// Returns `{"ok":true}` on success; on failure returns the gh stderr as the
// error body so the frontend can surface it.
fn gh_issue_close<R: tauri::Runtime>(
    app: &AppHandle<R>,
    number: u64,
    comment: &str,
) -> Result<Vec<u8>, String> {
    let root = project_root(Some(app)).ok_or_else(|| "no project root".to_string())?;
    let n = number.to_string();
    let mut args: Vec<&str> = vec!["issue", "close", &n];
    let trimmed = comment.trim();
    if !trimmed.is_empty() {
        args.push("-c");
        args.push(trimmed);
    }
    let out = std::process::Command::new("gh")
        .current_dir(&root)
        .args(&args)
        .output()
        .map_err(|e| format!("failed to spawn gh: {}", e))?;
    if out.status.success() {
        Ok(b"{\"ok\":true}".to_vec())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        eprintln!("[gh issue close {}] non-zero exit: {}", n, stderr);
        Err(stderr)
    }
}

fn close_issue_commit_comment(repo_slug: &str, full_sha: &str, subject: &str) -> String {
    let commit_url = format!("https://github.com/{}/commit/{}", repo_slug, full_sha);
    let trimmed_subject = subject.trim();
    if trimmed_subject.is_empty() {
        format!("Closed by {}", commit_url)
    } else {
        format!("Closed by {}\n\nCommit: {}", commit_url, trimmed_subject)
    }
}

fn issue_close_json_error(code: &str, issue: u64, sha: &str, message: String) -> Vec<u8> {
    serde_json::json!({
        "ok": false,
        "code": code,
        "issue": issue,
        "sha": sha,
        "message": message,
    })
    .to_string()
    .into_bytes()
}

fn git_full_commit_sha<R: tauri::Runtime>(app: &AppHandle<R>, sha: &str) -> Result<String, String> {
    let trimmed = sha.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid commit sha".to_string());
    }
    let rev = format!("{}^{{commit}}", trimmed);
    git_run(app, &["rev-parse", "--verify", &rev]).map(|s| s.trim().to_string())
}

fn git_commit_subject<R: tauri::Runtime>(
    app: &AppHandle<R>,
    full_sha: &str,
) -> Result<String, String> {
    git_run(app, &["show", "-s", "--format=%s", full_sha]).map(|s| s.trim().to_string())
}

fn gh_commit_visible<R: tauri::Runtime>(
    app: &AppHandle<R>,
    repo_slug: &str,
    full_sha: &str,
) -> Result<bool, String> {
    let root = project_root(Some(app)).ok_or_else(|| "no project root".to_string())?;
    let path = format!("repos/{}/commits/{}", repo_slug, full_sha);
    let out = std::process::Command::new("gh")
        .current_dir(&root)
        .args(["api", &path])
        .output()
        .map_err(|e| format!("failed to spawn gh: {}", e))?;
    if out.status.success() {
        Ok(true)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        eprintln!("[gh commit visible {}] non-zero exit: {}", full_sha, stderr);
        if gh_commit_missing_stderr(&stderr) {
            Ok(false)
        } else {
            Err(stderr)
        }
    }
}

fn gh_commit_missing_stderr(stderr: &str) -> bool {
    stderr.contains("HTTP 404")
        || stderr.contains("HTTP 422")
        || stderr.contains("Not Found")
        || stderr.contains("No commit found")
}

fn gh_issue_close_with_commit<R: tauri::Runtime>(
    app: &AppHandle<R>,
    number: u64,
    sha: &str,
    push_before_close: bool,
) -> (u16, &'static str, Vec<u8>) {
    let mut full_sha = match git_full_commit_sha(app, sha) {
        Ok(s) => s,
        Err(e) => {
            return (
                400,
                "application/json; charset=utf-8",
                issue_close_json_error("invalid-commit", number, sha, e),
            );
        }
    };
    let repo_slug = match repo_owner_name(app) {
        Some(slug) => slug,
        None => {
            return (
                400,
                "application/json; charset=utf-8",
                issue_close_json_error(
                    "no-github-remote",
                    number,
                    &full_sha,
                    "Cannot close with a commit link because origin is not a GitHub remote."
                        .to_string(),
                ),
            );
        }
    };
    if push_before_close {
        match push_focused_commit(app, &full_sha) {
            Ok(pushed_sha) => full_sha = pushed_sha,
            Err(e) => {
                return (
                    502,
                    "application/json; charset=utf-8",
                    issue_close_json_error("focused-push-failed", number, &full_sha, e),
                );
            }
        }
    }
    match gh_commit_visible(app, &repo_slug, &full_sha) {
        Ok(true) => {}
        Ok(false) => {
            let short_sha: String = full_sha.chars().take(7).collect();
            return (
                409,
                "application/json; charset=utf-8",
                issue_close_json_error(
                    "commit-not-visible",
                    number,
                    &full_sha,
                    format!(
                        "Committed {}, but did not close #{} because GitHub cannot see the commit yet. Retry the verified close helper after the push problem is resolved.",
                        short_sha, number
                    ),
                ),
            );
        }
        Err(e) => {
            return (
                502,
                "application/json; charset=utf-8",
                issue_close_json_error("commit-visibility-check-failed", number, &full_sha, e),
            );
        }
    }

    let subject = git_commit_subject(app, &full_sha).unwrap_or_default();
    let comment = close_issue_commit_comment(&repo_slug, &full_sha, &subject);
    match gh_issue_close(app, number, &comment) {
        Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
        Err(e) => {
            eprintln!("[gh issue close {}] {}", number, e);
            (500, "text/plain; charset=utf-8", e.into_bytes())
        }
    }
}

fn handle_issue_close_json<R: tauri::Runtime>(
    app: &AppHandle<R>,
    body: &[u8],
) -> (u16, &'static str, Vec<u8>) {
    let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    if parsed.is_null() {
        return (
            400,
            "application/json; charset=utf-8",
            br#"{"error":"malformed issue close JSON"}"#.to_vec(),
        );
    }
    let number = parsed.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
    if number == 0 {
        return (
            400,
            "application/json; charset=utf-8",
            br#"{"error":"missing number"}"#.to_vec(),
        );
    }
    let commit = parsed
        .get("commit")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let push_before_close = parsed
        .get("push")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let comment = parsed
        .get("comment")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !commit.is_empty() {
        return gh_issue_close_with_commit(app, number, &commit, push_before_close);
    }
    match gh_issue_close(app, number, &comment) {
        Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
        Err(e) => {
            eprintln!("[issue close intent number={}] {}", number, e);
            (500, "text/plain; charset=utf-8", e.into_bytes())
        }
    }
}

#[cfg(test)]
mod issue_close_tests {
    use super::{close_issue_commit_comment, gh_commit_missing_stderr};

    #[test]
    fn generated_commit_close_comment_uses_full_url_without_trailing_period() {
        let comment = close_issue_commit_comment(
            "judell/bram",
            "8b7c4407c0ffee00000000000000000000000000",
            "Fix draft-backed worklist history",
        );

        assert_eq!(
            comment,
            "Closed by https://github.com/judell/bram/commit/8b7c4407c0ffee00000000000000000000000000\n\nCommit: Fix draft-backed worklist history"
        );
        assert!(!comment.ends_with('.'));
    }

    #[test]
    fn generated_commit_close_comment_tolerates_missing_subject() {
        let comment = close_issue_commit_comment(
            "judell/bram",
            "8b7c4407c0ffee00000000000000000000000000",
            "",
        );

        assert_eq!(
            comment,
            "Closed by https://github.com/judell/bram/commit/8b7c4407c0ffee00000000000000000000000000"
        );
    }

    #[test]
    fn github_no_commit_422_is_not_visible() {
        assert!(gh_commit_missing_stderr(
            "gh: No commit found for SHA: abcdef1234567890 (HTTP 422)\n"
        ));
    }
}

fn issue_actor_label(value: &serde_json::Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let obj = value.as_object()?;
    for key in ["login", "name"] {
        let Some(s) = obj.get(key).and_then(|v| v.as_str()) else {
            continue;
        };
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn repo_owner_name<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<String> {
    let remote_url = git_run(app, &["remote", "get-url", "origin"]).ok()?;
    let html_base = remote_to_html(remote_url.trim());
    let slug = html_base.strip_prefix("https://github.com/")?;
    if slug.is_empty() {
        None
    } else {
        Some(slug.to_string())
    }
}

fn gh_issue_closed_event_actor<R: tauri::Runtime>(
    app: &AppHandle<R>,
    repo_slug: &str,
    number: u64,
) -> Option<(String, String)> {
    let root = project_root(Some(app))?;
    let path = format!("repos/{}/issues/{}/events", repo_slug, number);
    let out = std::process::Command::new("gh")
        .current_dir(&root)
        .args(["api", &path])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "[gh issue events {}] non-zero exit: {}",
            number,
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    let events: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    let mut latest: Option<(String, String)> = None;
    for event in events {
        if event.get("event").and_then(|v| v.as_str()) != Some("closed") {
            continue;
        }
        let actor = event.get("actor").and_then(issue_actor_label)?;
        let created_at = event.get("created_at").and_then(|v| v.as_str())?;
        let should_replace = latest
            .as_ref()
            .map(|(_, current_at)| created_at > current_at.as_str())
            .unwrap_or(true);
        if should_replace {
            latest = Some((actor, created_at.to_string()));
        }
    }
    latest
}

fn enrich_issue_activity<R: tauri::Runtime>(
    _app: &AppHandle<R>,
    issue: &mut serde_json::Value,
    repo_slug: Option<&str>,
) {
    let Some(obj) = issue.as_object_mut() else {
        return;
    };

    let mut latest_comment_at: Option<String> = None;
    let mut latest_comment_author: Option<String> = None;
    let mut comment_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    if let Some(comments) = obj.get("comments").and_then(|v| v.as_array()) {
        for comment in comments {
            let Some(created_at) = comment.get("createdAt").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(author) = comment.get("author").and_then(issue_actor_label) {
                *comment_counts.entry(author).or_insert(0) += 1;
            }
            let should_replace = latest_comment_at
                .as_deref()
                .map(|current| created_at > current)
                .unwrap_or(true);
            if should_replace {
                latest_comment_at = Some(created_at.to_string());
                latest_comment_author = comment
                    .get("author")
                    .and_then(issue_actor_label)
                    .or_else(|| Some(String::new()));
            }
        }
    }
    if !comment_counts.is_empty() {
        let mut entries: Vec<(String, usize)> = comment_counts.into_iter().collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let summary = entries
            .into_iter()
            .map(|(author, count)| format!("{}: {}", author, count))
            .collect::<Vec<_>>()
            .join(", ");
        obj.insert(
            "commentSummary".to_string(),
            serde_json::Value::String(summary),
        );
    }

    let activity_at = latest_comment_at
        .clone()
        .or_else(|| {
            obj.get("updatedAt")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            obj.get("createdAt")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });

    if let Some(activity_at) = activity_at {
        obj.insert(
            "activityAt".to_string(),
            serde_json::Value::String(activity_at),
        );
    }
    if let Some(latest_comment_at) = latest_comment_at {
        obj.insert(
            "latestCommentAt".to_string(),
            serde_json::Value::String(latest_comment_at),
        );
    }
    if let Some(latest_comment_author) = latest_comment_author {
        if !latest_comment_author.is_empty() {
            obj.insert(
                "latestCommentAuthor".to_string(),
                serde_json::Value::String(latest_comment_author),
            );
        }
    }
    let _ = repo_slug;
}

// Resolve the GitHub web URL of the configured origin remote. Used by the
// Issues tab's "New issue" button so the frontend doesn't have to parse the
// remote URL itself. Returns an empty string for both htmlBase and the
// composed URLs when there is no GitHub remote.
fn repo_origin_info<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<Vec<u8>, String> {
    let remote_url = git_run(app, &["remote", "get-url", "origin"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let html_base = remote_to_html(&remote_url);
    let issues_url = if html_base.is_empty() {
        String::new()
    } else {
        format!("{}/issues", html_base)
    };
    let issues_new_url = if html_base.is_empty() {
        String::new()
    } else {
        format!("{}/issues/new", html_base)
    };
    let info = serde_json::json!({
        "remoteUrl": remote_url,
        "htmlBase": html_base,
        "issuesUrl": issues_url,
        "issuesNewUrl": issues_new_url,
    });
    serde_json::to_vec(&info).map_err(|e| e.to_string())
}

fn remote_to_html(remote: &str) -> String {
    let r = remote.trim().trim_end_matches(".git");
    if let Some(rest) = r.strip_prefix("git@github.com:") {
        return format!("https://github.com/{}", rest);
    }
    if r.starts_with("https://github.com/") || r.starts_with("http://github.com/") {
        return r.to_string();
    }
    String::new()
}

fn git_commit_detail<R: tauri::Runtime>(app: &AppHandle<R>, sha: &str) -> Result<Vec<u8>, String> {
    if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid sha".to_string());
    }
    // numstat first, then patch — git lets us combine in one call.
    let numstat = git_run(app, &["show", "--format=", "--numstat", sha])?;
    let mut total_add: u64 = 0;
    let mut total_del: u64 = 0;
    let mut files: Vec<(String, u64, u64)> = Vec::new();
    for line in numstat.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() != 3 {
            continue;
        }
        let a: u64 = parts[0].parse().unwrap_or(0);
        let d: u64 = parts[1].parse().unwrap_or(0);
        total_add += a;
        total_del += d;
        files.push((parts[2].to_string(), a, d));
    }

    // Per-file patch via `git show -p --format= -- <file>` would be cleanest,
    // but that's N git invocations. Use one show and split on `diff --git`.
    let patch_all = git_run(app, &["show", "--format=", "-p", sha])?;
    let mut patches: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut current_file = String::new();
    let mut current_buf = String::new();
    for line in patch_all.lines() {
        if line.starts_with("diff --git ") {
            if !current_file.is_empty() {
                patches.insert(current_file.clone(), current_buf.clone());
            }
            current_buf.clear();
            current_buf.push_str(line);
            current_buf.push('\n');
            // Extract filename from "diff --git a/<path> b/<path>"
            // Use the b/ side for renames.
            if let Some(rest) = line.strip_prefix("diff --git ") {
                if let Some(b_idx) = rest.find(" b/") {
                    current_file = rest[b_idx + 3..].to_string();
                } else {
                    current_file.clear();
                }
            }
        } else {
            current_buf.push_str(line);
            current_buf.push('\n');
        }
    }
    if !current_file.is_empty() {
        patches.insert(current_file, current_buf);
    }

    let files_json: Vec<serde_json::Value> = files
        .iter()
        .map(|(name, a, d)| {
            serde_json::json!({
                "filename": name,
                "additions": a,
                "deletions": d,
                "patch": patches.get(name).cloned().unwrap_or_default(),
            })
        })
        .collect();
    // Full commit message (%B). Used by the right-pane expander so a
    // commit's body paragraphs are available without `git show` in a
    // terminal.
    let message = git_run(app, &["show", "-s", "--format=%B", sha])
        .unwrap_or_default()
        .trim_end_matches('\n')
        .to_string();
    let detail = serde_json::json!({
        "sha": sha,
        "stats": { "additions": total_add, "deletions": total_del },
        "files": files_json,
        "message": message,
    });
    serde_json::to_vec(&detail).map_err(|e| e.to_string())
}

fn bram_app_root_candidates(
    resource_dir: Option<PathBuf>,
    executable_dir: Option<PathBuf>,
    current_exe: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(resource_dir) = resource_dir {
        candidates.push(resource_dir.join("app"));
    }
    if let Some(executable_dir) = executable_dir {
        candidates.push(executable_dir.join("app"));
    }
    if let Some(exe) = current_exe {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("app"));
            candidates.push(dir.join("../Resources/app"));
        }
    }

    candidates
}

fn resolve_app_root<R: tauri::Runtime>(app: Option<&AppHandle<R>>) -> Option<PathBuf> {
    let resource_dir = app.and_then(|app| app.path().resource_dir().ok());
    let executable_dir = app.and_then(|app| app.path().executable_dir().ok());
    let current_exe = std::env::current_exe().ok();

    bram_app_root_candidates(resource_dir, executable_dir, current_exe)
        .into_iter()
        .find(|path| path.exists())
}

#[tauri::command]
fn pty_spawn(
    cmd: String,
    args: Vec<String>,
    cols: u16,
    rows: u16,
    on_data: Channel<Vec<u8>>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    let mut command = CommandBuilder::new(cmd);
    // Substitute placeholder paths under ./app/ with absolute paths into
    // the bundled app dir, so bash's --rcfile (and any future
    // app-relative arg) resolves correctly regardless of the project
    // root we set as the PTY's cwd. Falls back to extracting from the
    // embedded tree when no on-disk app/ is alongside the binary.
    for a in args {
        let resolved = if let Some(rest) = a.strip_prefix("./app/") {
            match extract_app_file(&app, rest) {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(e) => {
                    eprintln!("[pty_spawn] could not resolve {}: {}", a, e);
                    a
                }
            }
        } else {
            a
        };
        command.arg(resolved);
    }
    if let Some(root) = project_root(Some(&app)) {
        command.cwd(root);
    } else if let Some(home) = home_dir() {
        command.cwd(home);
    }
    for (k, v) in std::env::vars() {
        command.env(k, v);
    }
    command.env("TERM", "xterm-256color");
    if let Ok(hint_path) = active_agent_hint_path(&app) {
        if let Some(parent) = hint_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(&hint_path);
        let hint = hint_path.to_string_lossy().into_owned();
        command.env("BRAM_AGENT_HINT", hint.clone());
        command.env("XMLUI_DESKTOP_AGENT_HINT", hint);
    }
    if let Some(p) = LOOPBACK_PORT.get() {
        command.env("BRAM_PORT", p.to_string());
        command.env("XMLUI_DESKTOP_PORT", p.to_string());
    }
    // Propagate trace toggle + log path into the PTY child so hook
    // scripts (worklist-guard.py for Claude, worklist-guard-codex.py
    // for Codex) can write [hook] records into the same trace file as
    // the host. See trace-category-hook.
    if bram_trace_enabled() {
        command.env("BRAM_TRACE", "1");
        if let Some(path) = bram_trace_log_file(&app) {
            command.env("BRAM_TRACE_LOG", path.to_string_lossy().into_owned());
        }
    }

    let _child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| e.to_string())?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    *state.0.lock().unwrap() = Some(PtyState {
        master: pair.master,
        writer,
    });

    let app_for_thread = app.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // [pty-in] throttling state, per the issue #49 spec. Small
        // (<16-byte) reads inside a 50ms window collapse into one
        // summary line; larger reads flush the accumulator and log
        // individually. All state is thread-local — no locking.
        const SMALL_THRESHOLD: usize = 16;
        const SMALL_WINDOW_MS: u128 = 50;
        let mut small_bytes: usize = 0;
        let mut small_runs: usize = 0;
        let mut small_first_preview: String = String::new();
        let mut small_last: Option<std::time::Instant> = None;
        // Time of the previous `[pty-in]` trace emission, used to compute
        // `gap_ms=<n>` for each emit. Lets #78 analysis correlate
        // turn-end fires with the silence gap that triggered them — a
        // premature fire would show `gap_ms` just past the 800 ms
        // threshold; a real end-of-turn shows a much larger gap.
        let mut last_pty_in_emit_at: Option<std::time::Instant> = None;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    if bram_trace_enabled() && small_runs > 0 {
                        let gap_ms = last_pty_in_emit_at
                            .map(|t| t.elapsed().as_millis())
                            .unwrap_or(0);
                        append_bram_trace_line(
                            &app_for_thread,
                            "pty-in",
                            &format!(
                                "gap_ms={} bytes={} runs={} preview={}",
                                gap_ms, small_bytes, small_runs, small_first_preview
                            ),
                        );
                    }
                    break;
                }
                Ok(n) => {
                    if bram_trace_enabled() {
                        let data = &buf[..n];
                        if n >= SMALL_THRESHOLD {
                            // Flush any pending small-read accumulator
                            // first so the order in the log matches the
                            // order of arrivals.
                            if small_runs > 0 {
                                let gap_ms = last_pty_in_emit_at
                                    .map(|t| t.elapsed().as_millis())
                                    .unwrap_or(0);
                                append_bram_trace_line(
                                    &app_for_thread,
                                    "pty-in",
                                    &format!(
                                        "gap_ms={} bytes={} runs={} preview={}",
                                        gap_ms, small_bytes, small_runs, small_first_preview
                                    ),
                                );
                                last_pty_in_emit_at = Some(std::time::Instant::now());
                                small_bytes = 0;
                                small_runs = 0;
                                small_first_preview.clear();
                                small_last = None;
                            }
                            let preview = bram_trace_preview(&String::from_utf8_lossy(data), 80);
                            let gap_ms = last_pty_in_emit_at
                                .map(|t| t.elapsed().as_millis())
                                .unwrap_or(0);
                            append_bram_trace_line(
                                &app_for_thread,
                                "pty-in",
                                &format!("gap_ms={} bytes={} preview={}", gap_ms, n, preview),
                            );
                            last_pty_in_emit_at = Some(std::time::Instant::now());
                        } else {
                            let within_window = small_last
                                .map(|t| t.elapsed().as_millis() < SMALL_WINDOW_MS)
                                .unwrap_or(false);
                            if within_window {
                                small_bytes += n;
                                small_runs += 1;
                            } else {
                                if small_runs > 0 {
                                    let gap_ms = last_pty_in_emit_at
                                        .map(|t| t.elapsed().as_millis())
                                        .unwrap_or(0);
                                    append_bram_trace_line(
                                        &app_for_thread,
                                        "pty-in",
                                        &format!(
                                            "gap_ms={} bytes={} runs={} preview={}",
                                            gap_ms, small_bytes, small_runs, small_first_preview
                                        ),
                                    );
                                    last_pty_in_emit_at = Some(std::time::Instant::now());
                                }
                                small_bytes = n;
                                small_runs = 1;
                                small_first_preview =
                                    bram_trace_preview(&String::from_utf8_lossy(data), 80);
                            }
                            small_last = Some(std::time::Instant::now());
                        }
                    }
                    pty_menu_update(&app_for_thread, &buf[..n]);
                    pty_agent_turn_update(&app_for_thread, &buf[..n]);
                    pty_agent_status_update(&app_for_thread);
                    if pty_output_clears_inflight(&buf[..n]) {
                        clear_active_sentinel_with_reason(
                            &app_for_thread,
                            "pty-output-user-cancel",
                        );
                    }
                    if on_data.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    if bram_trace_enabled() && small_runs > 0 {
                        let gap_ms = last_pty_in_emit_at
                            .map(|t| t.elapsed().as_millis())
                            .unwrap_or(0);
                        append_bram_trace_line(
                            &app_for_thread,
                            "pty-in",
                            &format!(
                                "gap_ms={} bytes={} runs={} preview={}",
                                gap_ms, small_bytes, small_runs, small_first_preview
                            ),
                        );
                    }
                    break;
                }
            }
        }
    });

    // Optional auto-launch: type the configured agent at the bash
    // prompt. Bash buffers input until interactive; if it's still in
    // rcfile init the keystrokes queue and run as the first command.
    if let Some(root) = project_root(Some(&app)) {
        if let Some(cfg) = load_project_config(&root) {
            if let Some(agent) = cfg.shell.and_then(|s| s.agent) {
                let trimmed = agent.trim();
                if !trimmed.is_empty() {
                    let payload = format!("{}\r", trimmed);
                    let mut guard = state.0.lock().unwrap();
                    if let Some(pty) = guard.as_mut() {
                        if let Err(e) = pty.writer.write_all(payload.as_bytes()) {
                            eprintln!("[pty_spawn] failed to write agent autostart: {}", e);
                        }
                        let _ = pty.writer.flush();
                    }
                }
            }
        }
    }
    Ok(())
}

#[tauri::command]
fn pty_write(app: AppHandle, data: String, state: State<'_, AppState>) -> Result<(), String> {
    pty_write_internal(&app, &state, &data, "unknown")
}

// Shared body of `pty_write` so the disk-mediated relay (#86) can write
// queued intents through the same trace + menu-clear + auth-record
// pipeline as direct callers. `caller_hint` flows into the `[pty-out]`
// trace so we can distinguish direct writes from `pty-intent-*` drains
// at investigation time.
fn pty_write_internal<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &State<'_, AppState>,
    data: &str,
    caller_hint: &str,
) -> Result<(), String> {
    if bram_trace_enabled() && !data.is_empty() {
        append_bram_trace_line(
            app,
            "pty-out",
            &format!(
                "bytes={} preview={} is_structured={} caller_hint={}",
                data.len(),
                bram_trace_preview(data, 80),
                is_structured_intent_prefix(data),
                caller_hint,
            ),
        );
    }
    if !data.is_empty() {
        // \x1b[O (focus-out) and \x1b[I (focus-in) are pure focus-tracking
        // escape sequences that xterm.js emits as side effects of its
        // iframe gaining/losing focus — not user keystrokes. Dismissing
        // a permission menu on these dismisses it prematurely when the
        // user clicks a drawer menu button (which moves focus away from
        // the terminal). Skip the menu-clear for these specific 3-byte
        // sequences; still write them to the PTY (Claude Code may use
        // the focus signal). Closes #94.
        let is_focus_track = data == "\x1b[O" || data == "\x1b[I";
        if !is_focus_track {
            pty_menu_clear(app, data);
        } else if bram_trace_enabled() {
            let tool = pty_menu_cell()
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|m| m.tool.clone()))
                .unwrap_or_default();
            if !tool.is_empty() {
                append_bram_trace_line(
                    app,
                    "pty-menu",
                    &format!(
                        "state=preserved tool={} reason=focus-track preview={}",
                        tool,
                        bram_trace_preview(data, 16),
                    ),
                );
            }
        }
        record_worklist_authorization_from_input(app, data);
    }
    let mut guard = state.0.lock().unwrap();
    let pty = guard.as_mut().ok_or("pty not started")?;
    pty.writer
        .write_all(data.as_bytes())
        .map_err(|e| e.to_string())?;
    pty.writer.flush().map_err(|e| e.to_string())
}

// Disk-mediated relay for right-pane click intents (#86). The right-pane
// helpers (`toShell` / `toTurn` / `sendKeys` in `app/__shell/helpers.js`)
// call this instead of `pty_write` directly. Each invocation appends a
// JSONL line to `resources/.pty-intent.jsonl` then drains the queue
// under a process-wide mutex so concurrent calls can't race the
// read-then-truncate phase. Bracketed-paste framing for `kind:
// "toTurn"` is applied here (in the drain) so the right pane stays
// ignorant of PTY framing.
// Write a feedback draft to `resources/feedback-drafts/<id>.md` without
// routing through the PTY paste channel. Pulls long Iterate feedback off
// `toTurn`'s `\s+` whitespace collapse + downstream TUI paste buffer
// limits; the iterate payload then carries only a small `feedbackRef` and
// the agent reads the file directly. See #144.
#[tauri::command]
fn queue_feedback_draft<R: tauri::Runtime>(
    app: AppHandle<R>,
    payload: serde_json::Value,
) -> Result<(), String> {
    let feedback_id = payload
        .get("feedback_id")
        .and_then(|v| v.as_str())
        .ok_or("missing feedback_id")?
        .to_string();
    let text = payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let dir = feedback_drafts_dir(&app).ok_or("project root unknown".to_string())?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(format!("create feedback-drafts dir: {}", e));
    }
    let path = feedback_draft_path(&dir, &feedback_id).ok_or("invalid feedback_id".to_string())?;
    let bytes = text.len();
    std::fs::write(&path, text.as_bytes()).map_err(|e| format!("write feedback draft: {}", e))?;
    if bram_trace_enabled() {
        append_bram_trace_line(
            &app,
            "feedback-draft",
            &format!("op=write feedback_id={} bytes={}", feedback_id, bytes),
        );
    }
    Ok(())
}

#[tauri::command]
fn queue_pty_intent(
    app: AppHandle,
    payload: serde_json::Value,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let kind = payload
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or("missing kind")?
        .to_string();
    let data = payload
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or("missing data")?
        .to_string();
    if !matches!(kind.as_str(), "toShell" | "toTurn" | "sendKeys") {
        return Err(format!("unknown kind: {}", kind));
    }
    let Some(path) = pty_intent_file(&app) else {
        return Err("project root unknown".to_string());
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let seq = PTY_INTENT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let id = format!("intent-{}-{}", unix_now_ms(), seq);
    let line = serde_json::json!({
        "id": id,
        "kind": kind,
        "data": data,
        "at": unix_now_ms(),
    });
    let line_str = serde_json::to_string(&line).map_err(|e| e.to_string())?;

    let _drain_guard = pty_intent_lock().lock().map_err(|e| e.to_string())?;

    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        writeln!(file, "{}", line_str).map_err(|e| e.to_string())?;
    }
    if bram_trace_enabled() {
        append_bram_trace_line(
            &app,
            "pty-intent",
            &format!("op=enqueue id={} kind={} bytes={}", id, kind, data.len()),
        );
    }
    drain_pty_intents(&app, &state)
}

// Drains every queued intent in `resources/.pty-intent.jsonl` through
// `pty_write_internal` (preserves the [pty-out] trace + menu-clear +
// worklist-auth-record pipeline). On a PTY write failure, the failing
// line and all subsequent lines stay in the file for the next drain
// attempt; on success the file is removed. Caller must hold
// `pty_intent_lock()`.
fn drain_pty_intents<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &State<'_, AppState>,
) -> Result<(), String> {
    let Some(path) = pty_intent_file(app) else {
        return Ok(());
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            if bram_trace_enabled() {
                append_bram_trace_line(app, "pty-intent", "op=drain-read-failed");
            }
            return Ok(());
        }
    };
    if content.trim().is_empty() {
        let _ = std::fs::remove_file(&path);
        if bram_trace_enabled() {
            append_bram_trace_line(app, "pty-intent", "op=drain-empty");
        }
        return Ok(());
    }

    let mut wrote: usize = 0;
    let mut remaining: Vec<String> = Vec::new();
    let mut drain_error: Option<String> = None;

    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        if drain_error.is_some() {
            remaining.push(line.to_string());
            continue;
        }
        let intent: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let kind = intent.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let data = intent.get("data").and_then(|v| v.as_str()).unwrap_or("");
        let write_result = match kind {
            "toShell" => {
                let wrapped = format!("{}\n", data);
                pty_write_internal(app, state, &wrapped, "pty-intent-toShell")
            }
            "toTurn" => write_pty_turn_intent(app, state, data),
            "sendKeys" => pty_write_internal(app, state, data, "pty-intent-sendKeys"),
            _ => continue,
        };
        match write_result {
            Ok(()) => {
                wrote += 1;
            }
            Err(e) => {
                drain_error = Some(e);
                remaining.push(line.to_string());
            }
        }
    }

    if remaining.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        for line in &remaining {
            let _ = writeln!(file, "{}", line);
        }
    }

    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "pty-intent",
            &format!("op=drain wrote={} remaining={}", wrote, remaining.len()),
        );
    }

    match drain_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn write_pty_turn_intent<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &State<'_, AppState>,
    data: &str,
) -> Result<(), String> {
    if cfg!(windows) {
        pty_write_internal(app, state, "\x15", "pty-intent-toTurn-windows-clear")?;
        pty_write_internal(app, state, data, "pty-intent-toTurn-windows-payload")?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        pty_write_internal(app, state, "\r", "pty-intent-toTurn-windows-submit")
    } else {
        // Ctrl-U for traditional readline shells; \x1b (Escape) for CC's
        // Ink/React-based input which ignores Ctrl-U but treats Escape
        // as "discard current input." Belt and suspenders so partial
        // text typed into CC's input doesn't get concatenated with our
        // bracketed paste.
        let wrapped = format!("\x1b\x15\x1b[200~{}\x1b[201~\r", data);
        pty_write_internal(app, state, &wrapped, "pty-intent-toTurn")
    }
}

#[tauri::command]
fn pty_resize(cols: u16, rows: u16, state: State<'_, AppState>) -> Result<(), String> {
    let guard = state.0.lock().unwrap();
    let pty = guard.as_ref().ok_or("pty not started")?;
    pty.master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())
}

// --- Project-server (.bram.json / legacy .xmlui-desktop.json) -------------

fn load_project_config(root: &Path) -> Option<ProjectConfig> {
    for rel in [".bram.json", ".xmlui-desktop.json"] {
        let path = root.join(rel);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        return match serde_json::from_slice::<ProjectConfig>(&bytes) {
            Ok(cfg) => {
                eprintln!("[project-config] loaded {}", path.display());
                Some(cfg)
            }
            Err(e) => {
                eprintln!("[project-config] failed to parse {}: {}", path.display(), e);
                None
            }
        };
    }
    None
}

fn project_config_batch_commit_actions(config: Option<ProjectConfig>) -> bool {
    config
        .and_then(|c| c.worklist)
        .and_then(|w| w.batch_commit_actions)
        .unwrap_or(false)
}

// User-facing slice of .bram.json exposed to the Settings tab and the
// parent shell. Always returns a populated value (false / empty string)
// so the consumer never sees nulls; out-of-scope blocks like `server`
// stay invisible — they're project infrastructure, not Settings knobs.
fn settings_view_from_config(config: Option<ProjectConfig>) -> serde_json::Value {
    let (agent, batch, minimized) = match config {
        Some(c) => (
            c.shell.and_then(|s| s.agent).unwrap_or_default(),
            c.worklist
                .and_then(|w| w.batch_commit_actions)
                .unwrap_or(false),
            c.ui.and_then(|u| u.target_app_minimized).unwrap_or(false),
        ),
        None => (String::new(), false, false),
    };
    serde_json::json!({
        "shell": { "agent": agent },
        "worklist": { "batchCommitActions": batch },
        "ui": { "targetAppMinimized": minimized },
    })
}

fn is_project_config_path(path: &Path) -> bool {
    path.file_name()
        .map_or(false, |n| n == ".bram.json" || n == ".xmlui-desktop.json")
}

// Merge a Settings-tab POST body into the on-disk .bram.json, preserving
// any top-level keys the user may have hand-added (e.g. `server`) and
// nested keys we don't surface. The body is the same shape
// settings_view_from_config returns; we treat absent keys as "no change"
// rather than "set to default" — partial updates from the form are valid.
fn merge_settings_into_config(
    existing: serde_json::Value,
    update: &serde_json::Value,
) -> serde_json::Value {
    fn merge_obj(into: &mut serde_json::Map<String, serde_json::Value>, from: &serde_json::Value) {
        let Some(from_obj) = from.as_object() else {
            return;
        };
        for (k, v) in from_obj {
            match (into.get_mut(k), v) {
                (Some(existing_val), serde_json::Value::Object(_)) if existing_val.is_object() => {
                    if let Some(existing_map) = existing_val.as_object_mut() {
                        merge_obj(existing_map, v);
                    }
                }
                _ => {
                    into.insert(k.clone(), v.clone());
                }
            }
        }
    }
    let mut base = match existing {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    merge_obj(&mut base, update);
    serde_json::Value::Object(base)
}

fn settings_target_path(root: &Path) -> PathBuf {
    let bram = root.join(".bram.json");
    if bram.exists() {
        return bram;
    }
    let legacy = root.join(".xmlui-desktop.json");
    if legacy.exists() {
        return legacy;
    }
    bram
}

fn write_settings_to_config(root: &Path, update: &serde_json::Value) -> Result<(), String> {
    let path = settings_target_path(root);
    let existing: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("parse {}: {}", path.display(), e))?,
        Err(_) => serde_json::Value::Object(serde_json::Map::new()),
    };
    let merged = merge_settings_into_config(existing, update);
    let text = serde_json::to_string_pretty(&merged)
        .map_err(|e| format!("serialize {}: {}", path.display(), e))?;
    atomic_write_text(&path, &format!("{}\n", text))
}

fn is_port_listening(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

// Distinguishes a healthy reuse candidate from a wedged orphan. A bare TCP
// connect is not enough — a python -m http.server that was reparented to
// launchd after its Bram parent died accepts connects but never returns a
// response. Setup uses this to decide whether to reuse, log a loud warning,
// or spawn fresh.
enum PortStatus {
    Live,
    Unresponsive(String),
    NotListening,
}

fn probe_port_http(port: u16, path: &str) -> PortStatus {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
        Ok(s) => s,
        Err(_) => return PortStatus::NotListening,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let req_path = {
        let p = path.split('?').next().unwrap_or("/");
        if p.is_empty() {
            "/"
        } else {
            p
        }
    };
    let req = format!(
        "GET {} HTTP/1.0\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        req_path, port
    );
    if let Err(e) = stream.write_all(req.as_bytes()) {
        return PortStatus::Unresponsive(format!("write failed: {}", e));
    }
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(0) => PortStatus::Unresponsive("empty reply".into()),
        Ok(n) => {
            if buf[..n].starts_with(b"HTTP/") {
                PortStatus::Live
            } else {
                let preview = String::from_utf8_lossy(&buf[..n.min(40)]).to_string();
                PortStatus::Unresponsive(format!("non-HTTP reply: {:?}", preview))
            }
        }
        Err(e) => PortStatus::Unresponsive(format!("read failed: {}", e)),
    }
}

fn atomic_write_text(path: &Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("rename {} -> {}: {}", tmp.display(), path.display(), e))
}

fn bram_port_metadata_path(port_path: &Path) -> PathBuf {
    port_path
        .parent()
        .map(|p| p.join(".bram-port.json"))
        .unwrap_or_else(|| PathBuf::from(".bram-port.json"))
}

fn write_bram_port_files(proj: &Path, port: u16, started_at_ms: i64) -> Result<(), String> {
    let port_path = proj.join("resources/.bram-port");
    if let Some(parent) = port_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    atomic_write_text(&port_path, &port.to_string())?;
    let meta_path = bram_port_metadata_path(&port_path);
    let metadata = serde_json::json!({
        "schema": 1,
        "port": port,
        "pid": std::process::id(),
        "projectRoot": proj.to_string_lossy().to_string(),
        "startedAtMs": started_at_ms,
        "startedAt": format_iso_utc_ms(started_at_ms),
    });
    let metadata_text = serde_json::to_string_pretty(&metadata)
        .map_err(|e| format!("serialize port metadata: {}", e))?;
    atomic_write_text(&meta_path, &format!("{}\n", metadata_text))?;
    Ok(())
}

fn remove_bram_port_files(proj: &Path) {
    let port_path = proj.join("resources/.bram-port");
    let meta_path = bram_port_metadata_path(&port_path);
    let _ = std::fs::remove_file(port_path);
    let _ = std::fs::remove_file(meta_path);
}

fn wait_for_loopback_http(port: u16, total_ms: u64) -> bool {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(total_ms);
    while Instant::now() < deadline {
        if matches!(probe_port_http(port, "/__app-info"), PortStatus::Live) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

// Spawn the project's server per ServerConfig. Returns the Child on
// success. stdout/stderr are piped and forwarded to Bram's
// stderr with a `[server]` prefix. Caller is responsible for waiting
// on the port and storing the Child in state.
fn spawn_project_server(
    cfg: &ServerConfig,
    project_root: &Path,
) -> Result<std::process::Child, String> {
    let cwd = match cfg.cwd.as_deref() {
        Some(rel) => project_root.join(rel),
        None => project_root.to_path_buf(),
    };
    if !cwd.is_dir() {
        return Err(format!("cwd does not exist: {}", cwd.display()));
    }

    #[cfg(windows)]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(&cfg.command);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(&cfg.command);
        c
    };

    let mut child = command
        .current_dir(&cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn `{}`: {}", cfg.command, e))?;

    let pid = child.id();
    eprintln!(
        "[server] spawned pid={} cwd={} cmd={}",
        pid,
        cwd.display(),
        cfg.command
    );

    if let Some(stdout) = child.stdout.take() {
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("[server] {}", line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("[server] {}", line);
            }
        });
    }

    Ok(child)
}

// Block until the port answers or the timeout elapses. Returns true if
// the port came up. Used after spawn_project_server so we can warn (but
// not fail) if the server is taking longer than expected; the iframe
// loads eagerly and retries on its own.
fn wait_for_port(port: u16, total_ms: u64) -> bool {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(total_ms);
    while Instant::now() < deadline {
        if is_port_listening(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

// Reconcile Bram's runtime state with .bram.json, or the legacy
// .xmlui-desktop.json alias, after the file changes on disk. Kills the prior
// project-server child only when its command/cwd/port no longer match the
// file; otherwise we keep the running process and just refresh path/query.
// Always updates PaneUrlsState and emits right-pane-reload so main.js
// re-fetches the URL. Port changes do respawn, but the iframe origin shifts —
// service workers (XMLUI's apiInterceptor, MSW) won't rebind cleanly, so we
// log a warning telling the user to restart.
fn handle_project_config_reload<R: tauri::Runtime>(app_handle: &AppHandle<R>, proj_root: &Path) {
    let new_cfg = load_project_config(proj_root);
    let new_server = new_cfg.as_ref().and_then(|c| c.server.as_ref()).cloned();

    let mut spawned = false;
    let mut port_changed = false;
    {
        let state = app_handle.state::<SpawnedServerState>();
        let mut guard = state.0.lock().unwrap();
        let needs_respawn = match (&new_server, guard.as_ref()) {
            (Some(new), Some(cur)) => {
                new.command != cur.config.command
                    || new.cwd != cur.config.cwd
                    || new.port != cur.config.port
            }
            (Some(_), None) => true,
            (None, Some(_)) => true,
            (None, None) => false,
        };
        if needs_respawn {
            if let Some(mut cur) = guard.take() {
                port_changed = new_server.as_ref().map(|n| n.port) != Some(cur.config.port);
                let pid = cur.child.id();
                let _ = cur.child.kill();
                let _ = cur.child.wait();
                eprintln!("[server] killed pid={} on config reload", pid);
            }
            if let Some(cfg) = new_server.as_ref() {
                match spawn_project_server(cfg, proj_root) {
                    Ok(child) => {
                        *guard = Some(SpawnedServer {
                            child,
                            config: cfg.clone(),
                        });
                        spawned = true;
                    }
                    Err(e) => eprintln!("[server] respawn failed: {}", e),
                }
            }
        }
    }

    if spawned {
        if let Some(cfg) = new_server.as_ref() {
            if !wait_for_port(cfg.port, 5000) {
                eprintln!(
                    "[server] WARNING: respawned port {} did not come up within 5s; right-pane iframe will retry",
                    cfg.port
                );
            } else {
                eprintln!("[server] respawned; port {} is up", cfg.port);
            }
        }
    }

    if port_changed {
        eprintln!(
            "[server] WARNING: port changed via .bram.json; service workers were bound to the old origin and will not rebind cleanly — restart Bram to fully apply"
        );
    }

    // The iframe URL splices `cfg.path` into the /__project namespace,
    // and the upstream is the bare origin (http://host:port/). Both
    // change when the config changes; main.js re-reads them via
    // get_right_pane_url() on the right-pane-reload event below.
    let (new_right_pane_url, new_right_pane_upstream) = match new_server.as_ref() {
        Some(cfg) => {
            let path = if cfg.path.starts_with('/') {
                cfg.path.clone()
            } else {
                format!("/{}", cfg.path)
            };
            (
                format!("{}/__project{}", SHELL_ORIGIN, path),
                format!("http://localhost:{}/", cfg.port),
            )
        }
        None => {
            // Default fallback: iframe loads /__project/index.html and
            // the proxy forwards to the internal loopback (origin +
            // trailing slash). Derive both from default_right_pane.
            let default = {
                let state = app_handle.state::<PaneUrlsState>();
                let urls = state.0.lock().unwrap();
                urls.default_right_pane.clone()
            };
            let upstream = default
                .rsplit_once('/')
                .map(|(base, _)| format!("{}/", base))
                .unwrap_or_else(|| default.clone());
            (format!("{}/__project/index.html", SHELL_ORIGIN), upstream)
        }
    };
    {
        let state = app_handle.state::<PaneUrlsState>();
        let mut urls = state.0.lock().unwrap();
        urls.right_pane = new_right_pane_url.clone();
        urls.right_pane_upstream = new_right_pane_upstream.clone();
    }
    eprintln!(
        "[project-config] reloaded; right-pane url -> {} upstream -> {}",
        new_right_pane_url, new_right_pane_upstream
    );
    // Replayable: the parent-shell listener may not be ready when this
    // fires during early startup. Without a stored payload to replay at
    // iframe-attach, the iframe stays frozen on whatever it loaded
    // first. A "spurious" reload-on-attach is cheap (one refetch of the
    // already-current state) and is exactly what fixes the late-attach
    // frozen-iframe case. Refs #170 follow-up after the post-restart
    // regression at 2026-06-02T03:53Z, where two right-pane-reload
    // emits at startup were lost and the right pane stayed frozen
    // because the watcher then went idle.
    emit_replayable_signal(&app_handle, "right-pane-reload");

    emit_replayable_payload(
        app_handle,
        "settings-changed",
        settings_view_from_config(new_cfg),
    );
}

#[derive(serde::Serialize)]
struct WhisperStatusReport {
    running: bool,
    pid: Option<u32>,
}

#[tauri::command]
fn whisper_start(
    model_path: String,
    app: AppHandle,
    state: State<'_, WhisperState>,
) -> Result<u32, String> {
    let mut guard = state.0.lock().unwrap();
    if let Some(child) = guard.as_mut() {
        match child.try_wait() {
            Ok(None) => return Err("whisper-server already running".into()),
            Ok(Some(_)) => {}
            Err(_) => {}
        }
    }
    let model = expand_tilde(&model_path);
    // Keep whisper-server's transcoded WAV temp files outside the
    // watched project tree (otherwise the watcher fires right-pane-reload
    // mid-recording).
    let tmp_dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("no cache dir: {}", e))?
        .join("whisper");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
    let tmp_dir_str = tmp_dir.to_string_lossy().to_string();
    let candidates = ["whisper-server", "/opt/homebrew/bin/whisper-server"];
    let mut last_err = String::new();
    for bin in &candidates {
        match std::process::Command::new(bin)
            .arg("-m")
            .arg(&model)
            .arg("--convert")
            .arg("--tmp-dir")
            .arg(&tmp_dir_str)
            .arg("--port")
            .arg("18080")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                let pid = child.id();
                eprintln!(
                    "[whisper] spawned {} pid={} --port 18080 -m {} --tmp-dir {}",
                    bin, pid, model, tmp_dir_str
                );
                *guard = Some(child);
                return Ok(pid);
            }
            Err(e) => last_err = format!("{}: {}", bin, e),
        }
    }
    Err(format!("failed to spawn whisper-server: {}", last_err))
}

#[tauri::command]
fn whisper_stop(state: State<'_, WhisperState>) -> Result<(), String> {
    let mut guard = state.0.lock().unwrap();
    if let Some(mut child) = guard.take() {
        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("[whisper] killed pid={}", pid);
    }
    Ok(())
}

#[tauri::command]
fn whisper_status(state: State<'_, WhisperState>) -> WhisperStatusReport {
    let mut guard = state.0.lock().unwrap();
    if let Some(child) = guard.as_mut() {
        match child.try_wait() {
            Ok(None) => WhisperStatusReport {
                running: true,
                pid: Some(child.id()),
            },
            _ => {
                *guard = None;
                WhisperStatusReport {
                    running: false,
                    pid: None,
                }
            }
        }
    } else {
        WhisperStatusReport {
            running: false,
            pid: None,
        }
    }
}

#[tauri::command]
fn log_from_right_pane(app: AppHandle, payload: serde_json::Value) {
    // Iframe-side trace records arrive with `kind: "iframe-trace"` and a
    // `subkind` field; route them to the [iframe] category in the trace
    // log. Other payloads keep the existing stderr behavior so unrelated
    // iframe logging (e.g. git-push status from helpers.js) still shows
    // up at the command line.
    if payload.get("kind").and_then(|v| v.as_str()) == Some("iframe-trace") {
        if bram_trace_enabled() {
            let subkind = payload
                .get("subkind")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            // Render remaining fields (anything other than kind/subkind/at)
            // as a compact JSON object so the line stays scannable. `at`
            // is already captured by the outer [<ISO timestamp>]; the
            // iframe-side `at` is preserved inside the JSON for cases
            // where event-loop scheduling pushes the host's receive
            // moment well after the iframe's send moment.
            let mut rest = serde_json::Map::new();
            if let Some(obj) = payload.as_object() {
                for (k, v) in obj {
                    if k == "kind" || k == "subkind" {
                        continue;
                    }
                    rest.insert(k.clone(), v.clone());
                }
            }
            let rest_str = serde_json::to_string(&serde_json::Value::Object(rest))
                .unwrap_or_else(|_| "{}".to_string());
            append_bram_trace_line(&app, "iframe", &format!("subkind={} {}", subkind, rest_str));
        }
        return;
    }
    eprintln!("[right-pane] {}", payload);
}

#[tauri::command]
fn get_right_pane_url(state: State<'_, PaneUrlsState>) -> String {
    state.0.lock().unwrap().right_pane.clone()
}

#[tauri::command]
fn get_tools_pane_url(state: State<'_, PaneUrlsState>) -> String {
    state.0.lock().unwrap().tools.clone()
}

#[tauri::command]
fn open_devtools(window: tauri::WebviewWindow) {
    #[cfg(debug_assertions)]
    {
        if window.is_devtools_open() {
            window.close_devtools();
        } else {
            window.open_devtools();
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = window;
}

#[tauri::command]
fn git_push(app: AppHandle) -> Result<(), String> {
    let stderr = match git_run(&app, &["push"]) {
        Ok(_) => return Ok(()),
        Err(e) => e,
    };
    let is_nonff = stderr.contains("non-fast-forward") || stderr.contains("fetch first");
    if !is_nonff {
        return Err(stderr);
    }
    auto_rebase_and_push(&app).map_err(|e| format!("non-fast-forward; {}", e))
}

fn git_status_summary<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<Vec<u8>, String> {
    // Refresh the remote-tracking ref first; otherwise "behind" only
    // reflects the last fetch and the Pull button can be dimmed while
    // origin has new commits.
    git_run(app, &["fetch", "origin"])?;
    let ahead = git_run(app, &["rev-list", "--count", "@{u}..HEAD"])
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let behind = git_run(app, &["rev-list", "--count", "HEAD..@{u}"])
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let dirty = git_run(app, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    serde_json::to_vec(&serde_json::json!({
        "ahead": ahead,
        "behind": behind,
        "dirty": dirty,
    }))
    .map_err(|e| e.to_string())
}

// Rebase local commits on top of origin and retry push. Stashes any
// uncommitted working-tree changes first (rebase requires a clean
// tree) and pops the stash after, regardless of whether the rebase /
// push succeeded. If the stash pop has conflicts, the stash is left
// in place so the user can recover via `git stash list` / `git stash apply`.
fn auto_rebase_and_push<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let dirty = git_run(app, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let mut stashed = false;
    if dirty {
        git_run(
            app,
            &[
                "stash",
                "push",
                "--include-untracked",
                "-m",
                "bram-auto-rebase",
            ],
        )
        .map_err(|e| format!("auto-stash failed: {}", e))?;
        stashed = true;
    }

    let result: Result<(), String> = (|| {
        let branch =
            git_run(app, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|s| s.trim().to_string())?;
        git_run(app, &["fetch", "origin"])?;
        let upstream = format!("origin/{}", branch);
        match git_run(app, &["rebase", &upstream]) {
            Ok(_) => git_run(app, &["push"]).map(|_| ()),
            Err(rebase_err) => {
                let _ = git_run(app, &["rebase", "--abort"]);
                Err(format!(
                    "rebase conflicts (aborted, working tree clean — re-run the rebase manually or ask the agent, then push): {}",
                    rebase_err.trim()
                ))
            }
        }
    })();

    if stashed {
        if let Err(pop_err) = git_run(app, &["stash", "pop"]) {
            let prefix = result
                .as_ref()
                .err()
                .cloned()
                .unwrap_or_else(|| "push succeeded".to_string());
            return Err(format!(
                "{}; stash pop failed: {} (stash retained — recover with `git stash list` / `git stash apply`)",
                prefix,
                pop_err.trim()
            ));
        }
    }

    result
}

fn push_focused_commit<R: tauri::Runtime>(
    app: &AppHandle<R>,
    full_sha: &str,
) -> Result<String, String> {
    let branch =
        git_run(app, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|s| s.trim().to_string())?;
    if branch == "HEAD" || branch.is_empty() {
        return Err("refusing focused close push from a detached HEAD".to_string());
    }
    let head_sha = git_run(app, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string())?;
    if head_sha != full_sha {
        return Err(format!(
            "refusing focused close push because commit {} is not HEAD ({})",
            full_sha, head_sha
        ));
    }

    let dirty = git_run(app, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let mut stashed = false;
    if dirty {
        git_run(
            app,
            &[
                "stash",
                "push",
                "--include-untracked",
                "-m",
                "bram-focused-close-push",
            ],
        )
        .map_err(|e| format!("auto-stash failed: {}", e))?;
        stashed = true;
    }

    let result: Result<String, String> = (|| {
        git_run(app, &["fetch", "origin"])?;
        let upstream = format!("origin/{}", branch);
        if let Err(rebase_err) = git_run(app, &["rebase", &upstream]) {
            let _ = git_run(app, &["rebase", "--abort"]);
            return Err(format!(
                "rebase conflicts (aborted, working tree clean - re-run the rebase manually or ask the agent, then retry close): {}",
                rebase_err.trim()
            ));
        }
        let pushed_sha = git_run(app, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string())?;
        let refspec = format!("{}:refs/heads/{}", pushed_sha, branch);
        git_run(app, &["push", "origin", &refspec])?;
        Ok(pushed_sha)
    })();

    if stashed {
        if let Err(pop_err) = git_run(app, &["stash", "pop"]) {
            let prefix = result
                .as_ref()
                .err()
                .cloned()
                .unwrap_or_else(|| "focused push succeeded".to_string());
            return Err(format!(
                "{}; stash pop failed: {} (stash retained - recover with `git stash list` / `git stash apply`)",
                prefix,
                pop_err.trim()
            ));
        }
    }

    result
}

fn pull_rebase_with_autostash<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let dirty = git_run(app, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let mut stashed = false;
    if dirty {
        git_run(
            app,
            &[
                "stash",
                "push",
                "--include-untracked",
                "-m",
                "bram-pull-rebase",
            ],
        )
        .map_err(|e| format!("auto-stash failed: {}", e))?;
        stashed = true;
    }

    let result = git_run(app, &["pull", "--rebase"]).map(|_| ());
    if result.is_err() {
        let _ = git_run(app, &["rebase", "--abort"]);
    }

    if stashed {
        if let Err(pop_err) = git_run(app, &["stash", "pop"]) {
            let prefix = result
                .as_ref()
                .err()
                .cloned()
                .unwrap_or_else(|| "pull succeeded".to_string());
            return Err(format!(
                "{}; stash pop failed: {} (stash retained — recover with `git stash list` / `git stash apply`)",
                prefix,
                pop_err.trim()
            ));
        }
    }

    result
}

fn handle_git_pull_rebase<R: tauri::Runtime>(app: &AppHandle<R>) -> (u16, &'static str, Vec<u8>) {
    match pull_rebase_with_autostash(app) {
        Ok(_) => (
            200,
            "application/json; charset=utf-8",
            br#"{"ok":true}"#.to_vec(),
        ),
        Err(e) => {
            eprintln!("[http /__git/pull-rebase] {}", e);
            (500, "text/plain; charset=utf-8", e.into_bytes())
        }
    }
}

// Classify one diff line by leading char. Mirrors the iframe-side
// diffLineRows() in Globals.xs so the two stay aligned if the iframe
// fallback (unannotated) and the backend path ever render the same diff.
fn classify_diff_line(line: &str) -> &'static str {
    if line.starts_with("@@") {
        "hunk"
    } else if line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("diff ")
        || line.starts_with("index ")
    {
        "fileheader"
    } else if line.starts_with('+') {
        "add"
    } else if line.starts_with('-') {
        "del"
    } else {
        "context"
    }
}

// Word-diff the body of a paired (-line, +line) using `similar`, return
// per-side inline segments. The "+"/"-" prefix is stripped before the word
// diff so leading-sign changes don't show as emphasized.
//
// Each segment is { text, bg? } — bg is a theme variable string the
// renderer applies directly, "$color-{danger,success}-200" for the
// emphasized runs, omitted (transparent) for unchanged runs.
fn word_diff_segments(
    minus_line: &str,
    plus_line: &str,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    use similar::{ChangeTag, TextDiff};
    let a = minus_line.strip_prefix('-').unwrap_or(minus_line);
    let b = plus_line.strip_prefix('+').unwrap_or(plus_line);
    let diff = TextDiff::from_words(a, b);
    let mut minus_segs: Vec<serde_json::Value> = vec![serde_json::json!({ "text": "-" })];
    let mut plus_segs: Vec<serde_json::Value> = vec![serde_json::json!({ "text": "+" })];
    for change in diff.iter_all_changes() {
        let text = change.value().to_string();
        match change.tag() {
            ChangeTag::Equal => {
                minus_segs.push(serde_json::json!({ "text": text }));
                plus_segs.push(serde_json::json!({ "text": text }));
            }
            ChangeTag::Delete => {
                minus_segs.push(serde_json::json!({ "text": text, "bg": "$color-danger-200" }));
            }
            ChangeTag::Insert => {
                plus_segs.push(serde_json::json!({ "text": text, "bg": "$color-success-200" }));
            }
        }
    }
    (minus_segs, plus_segs)
}

// Annotate a unified-diff string with per-line classification and, for
// adjacent (one -, one +) pairs, intra-line word-diff segments. Returns
// a flat list of rows the DiffView consumes directly. Unequal -/+ block
// sizes get plain per-line rendering with no inline segments (no
// well-defined pairing).
// Skip word-diffing on diffs above this line count — past this size,
// per-line DOM cost (HStack-of-segments) outweighs the readability win,
// and the iframe-main-thread heartbeat (#dcef719) is what we have to
// protect. Plain per-line tinting still applies; word emphasis just
// turns off uniformly so the user gets a consistent fallback rather
// than partial coloring.
const DIFF_ANNOTATE_LINE_CAP: usize = 1500;

fn annotate_unified_diff(diff: &str) -> serde_json::Value {
    let lines: Vec<&str> = diff.split('\n').collect();
    let n = lines.len();
    let word_diff_enabled = n <= DIFF_ANNOTATE_LINE_CAP;
    let mut rows: Vec<serde_json::Value> = Vec::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        let line = lines[i];
        let kind = classify_diff_line(line);
        if kind == "del" && word_diff_enabled {
            // Collect run of consecutive del lines.
            let del_start = i;
            while i < n && classify_diff_line(lines[i]) == "del" {
                i += 1;
            }
            let del_end = i;
            // Then collect any immediately-following run of add lines.
            let add_start = i;
            while i < n && classify_diff_line(lines[i]) == "add" {
                i += 1;
            }
            let add_end = i;
            let del_count = del_end - del_start;
            let add_count = add_end - add_start;
            if del_count == add_count && del_count > 0 {
                // 1:1 pair lines by position, emit word-diff for each.
                for k in 0..del_count {
                    let (m_segs, p_segs) =
                        word_diff_segments(lines[del_start + k], lines[add_start + k]);
                    rows.push(serde_json::json!({
                        "kind": "del",
                        "text": lines[del_start + k],
                        "segments": m_segs,
                    }));
                    rows.push(serde_json::json!({
                        "kind": "add",
                        "text": lines[add_start + k],
                        "segments": p_segs,
                    }));
                }
            } else {
                // Unequal block: emit plain rows, no inline word-diff.
                for k in del_start..del_end {
                    rows.push(serde_json::json!({ "kind": "del", "text": lines[k] }));
                }
                for k in add_start..add_end {
                    rows.push(serde_json::json!({ "kind": "add", "text": lines[k] }));
                }
            }
        } else {
            rows.push(serde_json::json!({ "kind": kind, "text": line }));
            i += 1;
        }
    }
    serde_json::Value::Array(rows)
}

fn handle_diff_annotate(body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    let parsed: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                400,
                "text/plain; charset=utf-8",
                format!("invalid JSON: {}", e).into_bytes(),
            );
        }
    };
    let diff = parsed.get("diff").and_then(|v| v.as_str()).unwrap_or("");
    let rows = annotate_unified_diff(diff);
    let body = serde_json::to_vec(&rows).unwrap_or_default();
    (200, "application/json; charset=utf-8", body)
}

fn handle_settings_post<R: tauri::Runtime>(
    app: &AppHandle<R>,
    body: &[u8],
) -> (u16, &'static str, Vec<u8>) {
    let update: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                400,
                "text/plain; charset=utf-8",
                format!("invalid JSON: {}", e).into_bytes(),
            );
        }
    };
    let root = match project_root(Some(app)) {
        Some(r) => r,
        None => {
            return (
                500,
                "text/plain; charset=utf-8",
                b"project root unavailable".to_vec(),
            );
        }
    };
    if let Err(e) = write_settings_to_config(&root, &update) {
        return (500, "text/plain; charset=utf-8", e.into_bytes());
    }
    // Return the freshly merged view rather than echoing the POST body so
    // the form sees the same shape it gets from GET. The filesystem watcher
    // will also emit `settings-changed`; the duplicate refresh is harmless.
    let config = load_project_config(&root);
    let body = settings_view_from_config(config).to_string().into_bytes();
    (200, "application/json; charset=utf-8", body)
}

#[tauri::command]
fn open_url(url: String, app: AppHandle) -> Result<(), String> {
    // file:// URLs aren't permitted by the opener URL allowlist; route them
    // through open_path so the OS opens the file in its default app.
    if let Some(rest) = url.strip_prefix("file://") {
        // Strip an optional host (e.g. "file:///path" → host="" rest="/path";
        // "file://localhost/path" → strip "localhost" leaving "/path").
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        return app
            .opener()
            .open_path(path.to_string(), None::<String>)
            .map_err(|e| e.to_string());
    }
    app.opener()
        .open_url(url, None::<String>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn capture_screenshot<R: tauri::Runtime>(app: AppHandle<R>) -> Result<String, String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        return Err("screenshot capture is currently macOS-only".to_string());
    }

    #[cfg(target_os = "macos")]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();
        let cache = app.path().app_cache_dir().map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
        let out = cache.join(format!("screenshot-{}.png", ts));

        let status = std::process::Command::new("/usr/sbin/screencapture")
            .arg("-i")
            .arg(&out)
            .status()
            .map_err(|e| format!("failed to spawn screencapture: {}", e))?;
        if !status.success() {
            return Err(format!("screencapture exited with status {}", status));
        }
        if !out.exists() {
            // User pressed Esc during the interactive selection.
            return Err("cancelled".to_string());
        }
        eprintln!("[screenshot] wrote {}", out.display());
        Ok(out.to_string_lossy().into_owned())
    }
}

#[tauri::command]
fn save_trace_export(
    filename: String,
    content: String,
    mime_type: String,
) -> Result<String, String> {
    let safe_name = filename
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            _ => c,
        })
        .collect::<String>();

    let base_dir = home_dir()
        .map(|home| home.join("Downloads"))
        .filter(|path| path.exists())
        .or_else(|| std::env::current_dir().ok())
        .ok_or("could not resolve export directory")?;

    let target = base_dir.join(safe_name);
    std::fs::write(&target, content.as_bytes()).map_err(|e| e.to_string())?;
    eprintln!(
        "[trace-export] wrote {} bytes as {} to {}",
        content.len(),
        mime_type,
        target.display()
    );
    Ok(target.display().to_string())
}

#[derive(serde::Serialize)]
struct SessionEntry {
    id: String,
    mtime: u64,
    size: u64,
    title: Option<String>,
    provider: SessionProvider,
    current: bool,
}

#[derive(Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SessionProvider {
    Claude,
    Codex,
}

impl SessionProvider {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct SessionRecord {
    provider: SessionProvider,
    id: String,
    path: PathBuf,
    mtime: u64,
    size: u64,
    title: Option<String>,
}

#[derive(serde::Serialize)]
struct SessionsMeta {
    provider: SessionProvider,
    count: usize,
    current_id: Option<String>,
}

fn active_agent_hint_path<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let root = project_root(Some(app)).ok_or("could not resolve project root")?;
    let abs = strip_unc_prefix(root.canonicalize().map_err(|e| e.to_string())?);
    let encoded = encode_path_for_filename(&abs);
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    Ok(cache_dir
        .join("agent-hints")
        .join(format!("{}.json", encoded)))
}

fn hinted_session_provider<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<SessionProvider> {
    let path = active_agent_hint_path(app).ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    let record = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    SessionProvider::from_str(record.get("provider")?.as_str()?)
}

fn active_agent_hint_mtime<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<std::time::SystemTime> {
    let path = active_agent_hint_path(app).ok()?;
    std::fs::metadata(path).ok()?.modified().ok()
}

fn claude_sessions_dir<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let root = project_root(Some(app)).ok_or("could not resolve project root")?;
    let abs = strip_unc_prefix(root.canonicalize().map_err(|e| e.to_string())?);
    let encoded = encode_path_for_filename(&abs);
    let home = home_dir().ok_or("no HOME or USERPROFILE")?;
    Ok(home.join(".claude").join("projects").join(encoded))
}

// Best-effort label for a Claude session. Precedence (matches what
// `claude /resume` displays):
//   1. Most recent `custom-title` record — set via the rename surface
//      (rename_session() at lib.rs:2397). User-supplied, overrides everything.
//   2. Most recent `ai-title` record (field: `aiTitle`) — Claude Code itself
//      writes these auto-generated titles as the conversation evolves and
//      uses them in /resume listings. Walking to the latest one keeps XD in
//      sync with CC after compaction or topic shifts.
//   3. First `user` message snippet — last-resort fallback only used when
//      no title records exist yet (very fresh sessions before CC has
//      generated an ai-title).
// All title-record scans walk the whole file so a custom-title or ai-title
// appended after compaction still wins.
fn claude_session_title(path: &Path) -> std::io::Result<Option<String>> {
    let reader = BufReader::new(std::fs::File::open(path)?);
    let mut custom_title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut first_user: Option<String> = None;
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match record.get("type").and_then(|v| v.as_str()) {
            Some("custom-title") => {
                if let Some(t) = record.get("customTitle").and_then(|v| v.as_str()) {
                    custom_title = Some(t.to_string());
                }
            }
            Some("ai-title") => {
                if let Some(t) = record.get("aiTitle").and_then(|v| v.as_str()) {
                    ai_title = Some(t.to_string());
                }
            }
            Some("user") if first_user.is_none() => {
                if let Some(content) = record.pointer("/message/content") {
                    let text = match content {
                        serde_json::Value::String(s) => s.clone(),
                        _ => content.to_string(),
                    };
                    first_user = Some(text.chars().take(120).collect());
                }
            }
            _ => {}
        }
    }
    Ok(custom_title.or(ai_title).or(first_user))
}

fn claude_message_text(record: &serde_json::Value) -> String {
    let Some(content) = record.pointer("/message/content") else {
        return String::new();
    };
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|c| {
                if c.get("type").and_then(|v| v.as_str()) == Some("text") {
                    c.get("text").and_then(|v| v.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn codex_message_text(record: &serde_json::Value) -> Option<String> {
    if record.get("type").and_then(|v| v.as_str()) != Some("event_msg") {
        return None;
    }
    let payload = record.get("payload")?;
    match payload.get("type").and_then(|v| v.as_str()) {
        Some("user_message") | Some("agent_message") => payload
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => None,
    }
}

fn canonical_path_string(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn codex_session_index() -> Result<HashMap<String, String>, String> {
    let home = home_dir().ok_or("no HOME or USERPROFILE")?;
    let path = home.join(".codex").join("session_index.jsonl");
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e.to_string()),
    };
    let reader = BufReader::new(file);
    let mut titles = HashMap::new();
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = record.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(title) = record.get("thread_name").and_then(|v| v.as_str()) else {
            continue;
        };
        titles.insert(id.to_string(), title.to_string());
    }
    Ok(titles)
}

fn collect_codex_session_paths(dir: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            collect_codex_session_paths(&path, paths)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }
    Ok(())
}

fn codex_session_meta(path: &Path) -> std::io::Result<Option<(String, String)>> {
    let reader = BufReader::new(std::fs::File::open(path)?);
    for line in reader.lines().take(20) {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if record.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
            continue;
        }
        let Some(payload) = record.get("payload") else {
            continue;
        };
        let Some(id) = payload.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(cwd) = payload.get("cwd").and_then(|v| v.as_str()) else {
            continue;
        };
        return Ok(Some((id.to_string(), cwd.to_string())));
    }
    Ok(None)
}

fn codex_session_title(path: &Path) -> std::io::Result<Option<String>> {
    let reader = BufReader::new(std::fs::File::open(path)?);
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if record.get("type").and_then(|v| v.as_str()) != Some("event_msg") {
            continue;
        }
        let Some(payload) = record.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|v| v.as_str()) != Some("user_message") {
            continue;
        }
        let Some(message) = payload.get("message").and_then(|v| v.as_str()) else {
            continue;
        };
        return Ok(Some(message.chars().take(120).collect()));
    }
    Ok(None)
}

fn find_snippets(text: &str, q_lower: &str, max_count: usize) -> Vec<String> {
    let half: usize = 40;
    let text_lower = text.to_lowercase();
    let mut snippets: Vec<String> = Vec::new();
    let mut search_start: usize = 0;
    while snippets.len() < max_count && search_start < text.len() {
        let Some(rel) = text_lower[search_start..].find(q_lower) else {
            break;
        };
        let abs = search_start + rel;
        let mut s_start = abs.saturating_sub(half);
        while s_start < text.len() && !text.is_char_boundary(s_start) {
            s_start += 1;
        }
        let mut s_end = (abs + q_lower.len() + half).min(text.len());
        while s_end > 0 && !text.is_char_boundary(s_end) {
            s_end -= 1;
        }
        if s_start >= s_end {
            break;
        }
        let snippet: String = text[s_start..s_end]
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        snippets.push(snippet);
        search_start = abs + q_lower.len();
    }
    snippets
}

fn discover_claude_sessions<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<Vec<SessionRecord>, String> {
    let dir = claude_sessions_dir(app)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };

    let mut sessions = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let metadata = entry.metadata().map_err(|e| e.to_string())?;
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let title = claude_session_title(&path).ok().flatten();
        sessions.push(SessionRecord {
            provider: SessionProvider::Claude,
            id,
            path,
            mtime,
            size: metadata.len(),
            title,
        });
    }
    sessions.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    Ok(sessions)
}

fn discover_codex_sessions<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<Vec<SessionRecord>, String> {
    let project = project_root(Some(app)).ok_or("could not resolve project root")?;
    let project_cwd = canonical_path_string(&project);
    let home = home_dir().ok_or("no HOME or USERPROFILE")?;
    let sessions_root = home.join(".codex").join("sessions");
    let titles = codex_session_index()?;
    let mut paths = Vec::new();
    collect_codex_session_paths(&sessions_root, &mut paths)?;

    let mut sessions = Vec::new();
    for path in paths {
        let Some((id, cwd)) = codex_session_meta(&path).map_err(|e| e.to_string())? else {
            continue;
        };
        if canonical_path_string(Path::new(&cwd)) != project_cwd {
            continue;
        }
        let metadata = path.metadata().map_err(|e| e.to_string())?;
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let title = titles
            .get(&id)
            .cloned()
            .or_else(|| codex_session_title(&path).ok().flatten());
        sessions.push(SessionRecord {
            provider: SessionProvider::Codex,
            id: id.clone(),
            path,
            mtime,
            size: metadata.len(),
            title,
        });
    }
    sessions.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    Ok(sessions)
}

fn choose_session_provider(
    preferred: Option<SessionProvider>,
    claude: &[SessionRecord],
    codex: &[SessionRecord],
) -> SessionProvider {
    if let Some(provider) = preferred {
        return provider;
    }
    match (codex.first(), claude.first()) {
        (Some(codex_latest), Some(claude_latest)) if codex_latest.mtime >= claude_latest.mtime => {
            SessionProvider::Codex
        }
        (Some(_), None) => SessionProvider::Codex,
        _ => SessionProvider::Claude,
    }
}

fn sessions_for_provider<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> Result<(SessionProvider, Vec<SessionRecord>), String> {
    let claude = discover_claude_sessions(app)?;
    let codex = discover_codex_sessions(app)?;
    let preferred = preferred.or_else(|| hinted_session_provider(app));
    let provider = choose_session_provider(preferred, &claude, &codex);
    let sessions = match provider {
        SessionProvider::Claude => claude,
        SessionProvider::Codex => codex,
    };
    Ok((provider, sessions))
}

fn session_meta<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> Result<SessionsMeta, String> {
    let (provider, sessions) = sessions_for_provider(app, preferred)?;
    Ok(SessionsMeta {
        provider,
        count: sessions.len(),
        current_id: sessions.first().map(|s| s.id.clone()),
    })
}

fn search_sessions<R: tauri::Runtime>(
    app: &AppHandle<R>,
    query: &str,
    limit: usize,
    preferred: Option<SessionProvider>,
) -> Result<Vec<serde_json::Value>, String> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let q_lower = q.to_lowercase();
    let (provider, mut sessions) = sessions_for_provider(app, preferred)?;
    sessions.truncate(limit);

    let mut results: Vec<serde_json::Value> = Vec::new();
    for session in sessions {
        let Ok(content) = std::fs::read_to_string(&session.path) else {
            continue;
        };
        let mut all_text = String::new();
        for line in content.lines() {
            let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let text = match provider {
                SessionProvider::Claude => {
                    let role = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if role != "user" && role != "assistant" {
                        continue;
                    }
                    claude_message_text(&record)
                }
                SessionProvider::Codex => match codex_message_text(&record) {
                    Some(text) => text,
                    None => continue,
                },
            };
            if !text.is_empty() {
                all_text.push_str(&text);
                all_text.push('\n');
            }
        }
        let snippets = find_snippets(&all_text, &q_lower, 3);
        if !snippets.is_empty() {
            results.push(serde_json::json!({
                "id": session.id,
                "title": session.title,
                "mtime": session.mtime,
                "size": session.size,
                "provider": session.provider,
                "current": false,
                "snippets": snippets,
            }));
        }
    }
    Ok(results)
}

fn list_sessions<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> Result<Vec<SessionEntry>, String> {
    let (provider, sessions) = sessions_for_provider(app, preferred)?;
    // For Claude sessions, mark the live one using the same hysteresis-
    // backed picker that the Transcript pane uses. For Codex (or anything else),
    // fall back to "newest mtime" via idx == 0.
    let live_claude_id: Option<String> = match provider {
        SessionProvider::Claude => latest_claude_session_path(app)
            .ok()
            .flatten()
            .and_then(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            }),
        _ => None,
    };
    Ok(sessions
        .into_iter()
        .enumerate()
        .map(|(idx, session)| {
            let current = match &live_claude_id {
                Some(live_id) => session.id == *live_id,
                None => idx == 0,
            };
            SessionEntry {
                id: session.id,
                mtime: session.mtime,
                size: session.size,
                title: session.title,
                provider: session.provider,
                current,
            }
        })
        .collect())
}

fn read_session<R: tauri::Runtime>(
    app: &AppHandle<R>,
    id: &str,
    preferred: Option<SessionProvider>,
) -> Result<Vec<u8>, String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("invalid session id".to_string());
    }
    let (_, sessions) = sessions_for_provider(app, preferred)?;
    let session = sessions
        .into_iter()
        .find(|session| session.id == id)
        .ok_or("session not found")?;
    std::fs::read(&session.path).map_err(|e| e.to_string())
}

// Remove a session's JSONL file. Validates the id is a safe filename
// (alphanumeric + hyphen, same as read_session) and resolves the path
// via sessions_for_provider so we never touch anything outside the
// session dirs.
fn delete_session<R: tauri::Runtime>(
    app: &AppHandle<R>,
    id: &str,
    preferred: Option<SessionProvider>,
) -> Result<Vec<u8>, String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("invalid session id".to_string());
    }
    let (_, sessions) = sessions_for_provider(app, preferred)?;
    let session = sessions
        .into_iter()
        .find(|session| session.id == id)
        .ok_or("session not found")?;
    std::fs::remove_file(&session.path).map_err(|e| e.to_string())?;
    Ok(b"{\"ok\":true}".to_vec())
}

// Format `SystemTime::now()` as an RFC3339 string in UTC (seconds precision,
// no subseconds). Used for the `updated_at` field codex writes into
// session_index.jsonl entries. Bram has no date-formatting crate
// dependency; this inline implementation uses Howard Hinnant's gregorian
// algorithm to avoid adding one. Codex does not parse `updated_at` back
// (only `id` + `thread_name` are read), but writing a real RFC3339 keeps the
// file compatible with codex's own writers.
fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86400);
    let secs_in_day = secs.rem_euclid(86400);
    let hour = (secs_in_day / 3600) as u32;
    let minute = ((secs_in_day / 60) % 60) as u32;
    let second = (secs_in_day % 60) as u32;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as i64;
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month as u32, day, hour, minute, second
    )
}

// Rename a session. For Claude: append `{type: "custom-title", customTitle: ...}`
// to the session JSONL so claude_session_title at lib.rs:1893 picks it up on
// next read. For codex: append `{id, thread_name, updated_at}` to
// ~/.codex/session_index.jsonl (append-only, last entry wins) so both
// codex_session_index in Bram and codex's own session listing see the
// new title. Codex contract verified against codex-rs/rollout/src/session_index.rs.
fn rename_session<R: tauri::Runtime>(
    app: &AppHandle<R>,
    id: &str,
    preferred: Option<SessionProvider>,
    title: &str,
) -> Result<Vec<u8>, String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("invalid session id".to_string());
    }
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err("empty title".to_string());
    }
    let (provider, sessions) = sessions_for_provider(app, preferred)?;
    let session = sessions
        .into_iter()
        .find(|session| session.id == id)
        .ok_or("session not found")?;
    use std::io::Write;
    match provider {
        SessionProvider::Claude => {
            let record = serde_json::json!({ "type": "custom-title", "customTitle": trimmed });
            let mut line = serde_json::to_string(&record).map_err(|e| e.to_string())?;
            line.push('\n');
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&session.path)
                .map_err(|e| e.to_string())?;
            f.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        }
        SessionProvider::Codex => {
            let home = home_dir().ok_or("no HOME or USERPROFILE")?;
            let index_path = home.join(".codex").join("session_index.jsonl");
            if let Some(parent) = index_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let record = serde_json::json!({
                "id": session.id,
                "thread_name": trimmed,
                "updated_at": rfc3339_now(),
            });
            let mut line = serde_json::to_string(&record).map_err(|e| e.to_string())?;
            line.push('\n');
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&index_path)
                .map_err(|e| e.to_string())?;
            f.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        }
    }
    Ok(b"{\"ok\":true}".to_vec())
}

fn read_latest_session<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> Result<Vec<u8>, String> {
    let Some(path) = latest_session_path(app, preferred)? else {
        return Ok(Vec::new());
    };
    std::fs::read(&path).map_err(|e| e.to_string())
}

// Fast lookup for the most-recent Claude session JSONL by mtime alone.
// Avoids `discover_claude_sessions` which reads each file to extract a
// title — fine for the Sessions tab but catastrophic to call on every
// pending-poll (60+ sessions × multi-MB each = ~1GB of disk I/O per
// call). Latest-* endpoints only need the path; titles are unused.
fn latest_claude_session_path<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<Option<std::path::PathBuf>, String> {
    let dir = claude_sessions_dir(app)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    // Collect every jsonl with its mtime in one pass.
    let mut all: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = metadata.modified() else {
            continue;
        };
        all.push((mtime, path));
    }
    if all.is_empty() {
        return Ok(None);
    }
    // Raw best = newest mtime.
    let (raw_best_mtime, raw_best_path) = all
        .iter()
        .max_by_key(|(t, _)| *t)
        .cloned()
        .expect("non-empty");
    // Hysteresis: prefer the previously-chosen path unless it's clearly stale.
    let cache_cell = LIVE_CLAUDE_SESSION.get_or_init(|| Mutex::new(None));
    let mut cached = cache_cell.lock().map_err(|e| e.to_string())?;
    let chosen: PathBuf = match cached.as_ref() {
        Some((cached_path, _)) => {
            // Look up the cached path's current mtime in our scan.
            let cached_now = all.iter().find(|(_, p)| p == cached_path).map(|(t, _)| *t);
            match cached_now {
                None => {
                    // (a) cached path no longer exists → switch.
                    raw_best_path.clone()
                }
                Some(cached_mtime) => {
                    let now = std::time::SystemTime::now();
                    let cached_age = now
                        .duration_since(cached_mtime)
                        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
                    let raw_age = now
                        .duration_since(raw_best_mtime)
                        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
                    let different = &raw_best_path != cached_path;
                    let cond_b = cached_age > std::time::Duration::from_secs(30)
                        && raw_best_mtime > cached_mtime;
                    let cond_c = raw_age < std::time::Duration::from_secs(5)
                        && cached_age > std::time::Duration::from_secs(5);
                    if different && (cond_b || cond_c) {
                        raw_best_path.clone()
                    } else {
                        cached_path.clone()
                    }
                }
            }
        }
        None => raw_best_path.clone(),
    };
    // Record current mtime for the chosen path (could be raw_best or cached).
    let chosen_mtime = all
        .iter()
        .find(|(_, p)| p == &chosen)
        .map(|(t, _)| *t)
        .unwrap_or(raw_best_mtime);
    *cached = Some((chosen.clone(), chosen_mtime));
    Ok(Some(chosen))
}

fn latest_codex_session_path<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<Option<std::path::PathBuf>, String> {
    let project = project_root(Some(app)).ok_or("could not resolve project root")?;
    let project_cwd = canonical_path_string(&project);
    let home = home_dir().ok_or("no HOME or USERPROFILE")?;
    let sessions_root = home.join(".codex").join("sessions");
    let mut paths = Vec::new();
    collect_codex_session_paths(&sessions_root, &mut paths)?;
    let mut all: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for path in paths {
        let Some((_, cwd)) = codex_session_meta(&path).map_err(|e| e.to_string())? else {
            continue;
        };
        if canonical_path_string(Path::new(&cwd)) != project_cwd {
            continue;
        }
        let Ok(metadata) = path.metadata() else {
            continue;
        };
        let Ok(mtime) = metadata.modified() else {
            continue;
        };
        all.push((mtime, path));
    }
    if all.is_empty() {
        return Ok(None);
    }
    let (raw_best_mtime, raw_best_path) = all
        .iter()
        .max_by_key(|(t, _)| *t)
        .cloned()
        .expect("non-empty");
    let cache_cell = LIVE_CODEX_SESSION.get_or_init(|| Mutex::new(None));
    let mut cached = cache_cell.lock().map_err(|e| e.to_string())?;
    let chosen: PathBuf = match cached.as_ref() {
        Some((cached_path, _)) => {
            let cached_now = all.iter().find(|(_, p)| p == cached_path).map(|(t, _)| *t);
            match cached_now {
                None => raw_best_path.clone(),
                Some(cached_mtime) => {
                    let now = std::time::SystemTime::now();
                    let cached_age = now
                        .duration_since(cached_mtime)
                        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
                    let raw_age = now
                        .duration_since(raw_best_mtime)
                        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
                    let different = &raw_best_path != cached_path;
                    let cond_b = cached_age > std::time::Duration::from_secs(30)
                        && raw_best_mtime > cached_mtime;
                    let cond_c = raw_age < std::time::Duration::from_secs(5)
                        && cached_age > std::time::Duration::from_secs(5);
                    if different && (cond_b || cond_c) {
                        raw_best_path.clone()
                    } else {
                        cached_path.clone()
                    }
                }
            }
        }
        None => raw_best_path.clone(),
    };
    let chosen_mtime = all
        .iter()
        .find(|(_, p)| p == &chosen)
        .map(|(t, _)| *t)
        .unwrap_or(raw_best_mtime);
    *cached = Some((chosen.clone(), chosen_mtime));
    Ok(Some(chosen))
}

fn latest_session_path<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> Result<Option<std::path::PathBuf>, String> {
    let hinted = preferred.or_else(|| hinted_session_provider(app));
    let Some(provider) = hinted else {
        return Ok(None);
    };
    let path = match provider {
        SessionProvider::Claude => latest_claude_session_path(app)?,
        SessionProvider::Codex => latest_codex_session_path(app)?,
    };
    let Some(path) = path else {
        return Ok(None);
    };
    let Some(hint_mtime) = active_agent_hint_mtime(app) else {
        return Ok(None);
    };
    let session_mtime = match std::fs::metadata(&path).and_then(|md| md.modified()) {
        Ok(mtime) => mtime,
        Err(_) => return Ok(None),
    };
    if session_mtime < hint_mtime {
        return Ok(None);
    }
    Ok(Some(path))
}

fn system_time_ms(t: std::time::SystemTime) -> Option<i64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

fn start_codex_session_poll_fallback<R: tauri::Runtime>(app_handle: AppHandle<R>) {
    std::thread::spawn(move || {
        let mut last_seen: Option<(PathBuf, std::time::SystemTime)> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let path = match latest_session_path(&app_handle, Some(SessionProvider::Codex)) {
                Ok(Some(path)) => path,
                Ok(None) => {
                    last_seen = None;
                    continue;
                }
                Err(_) => continue,
            };
            let mtime = match std::fs::metadata(&path).and_then(|md| md.modified()) {
                Ok(mtime) => mtime,
                Err(_) => continue,
            };
            let advanced = match last_seen.as_ref() {
                Some((prev_path, prev_mtime)) if prev_path == &path => mtime > *prev_mtime,
                Some(_) | None => false,
            };
            last_seen = Some((path.clone(), mtime));
            if !advanced {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let mtime_ms = system_time_ms(mtime).unwrap_or(0);
            append_bram_trace_line(
                &app_handle,
                "jsonl-poll",
                &format!("provider=codex file={} mtime={}", name, mtime_ms),
            );
            trace_jsonl_detector_handoff(&app_handle, "codex-poll", &path, mtime_ms);
            check_jsonl_for_turn_end(&app_handle, &path);
            emit_replayable_signal(&app_handle, "talk-session-changed");
        }
    });
}

// Tail variant: return only the last N records of the JSONL. Lets Transcript
// poll aggressively without round-tripping the entire (multi-MB) file.
// Uses a seek-from-EOF, read-backward-in-chunks loop so server cost is
// proportional to N, not file size.
fn read_latest_session_tail<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
    lines: usize,
) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = latest_session_path(app, preferred)? else {
        return Ok(Vec::new());
    };
    let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let file_size = file.metadata().map_err(|e| e.to_string())?.len();
    if file_size == 0 || lines == 0 {
        return Ok(Vec::new());
    }
    // Need `lines + 1` newlines walking back from EOF to delimit `lines`
    // records (the +1 accounts for the trailing newline of the previous
    // record). If the file has fewer newlines we just start at offset 0.
    let target_newlines = lines + 1;
    let chunk_size: u64 = 64 * 1024;
    let mut buf = vec![0u8; chunk_size as usize];
    let mut pos: u64 = file_size;
    let mut newlines_seen: usize = 0;
    let mut start_offset: u64 = 0;
    while pos > 0 {
        let read_size = chunk_size.min(pos);
        pos -= read_size;
        file.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
        file.read_exact(&mut buf[..read_size as usize])
            .map_err(|e| e.to_string())?;
        let mut done = false;
        for i in (0..read_size as usize).rev() {
            if buf[i] == b'\n' {
                newlines_seen += 1;
                if newlines_seen >= target_newlines {
                    start_offset = pos + i as u64 + 1;
                    done = true;
                    break;
                }
            }
        }
        if done {
            break;
        }
    }
    file.seek(SeekFrom::Start(start_offset))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity((file_size - start_offset) as usize);
    file.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

const LATEST_TAIL_MAX_BYTES: usize = 256 * 1024;

fn cap_latest_tail_payload(content: Vec<u8>) -> (Vec<u8>, bool) {
    if content.len() <= LATEST_TAIL_MAX_BYTES {
        return (content, false);
    }
    let start = content.len().saturating_sub(LATEST_TAIL_MAX_BYTES);
    let Some(first_newline) = content[start..].iter().position(|b| *b == b'\n') else {
        return (Vec::new(), true);
    };
    (content[start + first_newline + 1..].to_vec(), true)
}

// Detect whether the latest session has a pending tool_use awaiting
// permission. Returns JSON describing the tool, or `{"pending":null}`
// when not pending. Reads ~32KB of the file's tail so the walk-back
// can find a complete most-recent record even when it contains a
// large Edit/MultiEdit tool_use (10-15KB is not unusual). DO NOT
// shrink below ~16KB: text.lines().rev() needs the start of the
// latest record to be in the buffer, otherwise the leading partial
// line fails JSON parse and the walk lands on an older record —
// producing false negatives on the menu.
fn read_latest_session_pending<R: tauri::Runtime>(
    app: &AppHandle<R>,
    _preferred: Option<SessionProvider>,
) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let _start = std::time::Instant::now();
    let Some(path) = latest_claude_session_path(app)? else {
        return Ok(br#"{"pending":null}"#.to_vec());
    };
    let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let file_size = file.metadata().map_err(|e| e.to_string())?.len();
    let want: u64 = 32 * 1024;
    let read_from = file_size.saturating_sub(want);
    file.seek(SeekFrom::Start(read_from))
        .map_err(|e| e.to_string())?;
    let mut tail = Vec::with_capacity((file_size - read_from) as usize);
    file.read_to_end(&mut tail).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&tail);
    // Walk newest-first. Collect tool_result.tool_use_id values from user
    // records into `resolved`, then when we find an assistant record with
    // tool_use blocks, return the FIRST unresolved one. This handles the
    // multi-tool-batch case: claude proposes A+B in one turn, user approves
    // A, tool_result A is the latest user record but B is still pending.
    let mut pending: Option<serde_json::Value> = None;
    let mut resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_break_reason: &'static str = "no-record";
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let typ = r.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if typ != "assistant" && typ != "user" {
            continue;
        }
        let content = r
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array());
        if typ == "user" {
            // Collect tool_result.tool_use_id from this user record and
            // keep walking back. A user record with NO tool_result content
            // (a genuine user message) means there's no pending tool
            // because the user has already responded — break.
            let Some(arr) = content else {
                last_break_reason = "user-no-content";
                break;
            };
            let mut had_tool_result = false;
            for c in arr {
                if c.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    had_tool_result = true;
                    if let Some(id) = c.get("tool_use_id").and_then(|v| v.as_str()) {
                        resolved.insert(id.to_string());
                    }
                }
            }
            if !had_tool_result {
                last_break_reason = "user-no-tool-result";
                break;
            }
            continue;
        }
        if content.is_none() {
            last_break_reason = "assistant-no-content";
            break;
        }
        let arr = content.unwrap();
        let has_tool_use = arr
            .iter()
            .any(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
        if !has_tool_use {
            last_break_reason = "assistant-no-tool-use";
            break;
        }
        // Return the first tool_use whose id is NOT in `resolved`.
        for c in arr {
            if c.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if resolved.contains(id) {
                continue;
            }
            pending = Some(c.clone());
            break;
        }
        last_break_reason = if pending.is_some() {
            "tool-use-found"
        } else {
            "all-tools-resolved"
        };
        break;
    }
    // Always log the outcome (cheap; helps diagnose missing-menu reports).
    let tool_name = pending
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("");
    eprintln!(
        "[pending-endpoint] reason={} tool={} tail_bytes={}",
        last_break_reason,
        tool_name,
        file_size - read_from,
    );
    let body = serde_json::json!({ "pending": pending });
    let result = serde_json::to_vec(&body).map_err(|e| e.to_string());
    let elapsed = _start.elapsed();
    if elapsed > std::time::Duration::from_millis(10) {
        eprintln!(
            "[pending-endpoint] slow: {}ms (file_size={}, tail_read={})",
            elapsed.as_millis(),
            file_size,
            file_size - read_from
        );
    }
    result
}

#[derive(Clone, Debug)]
struct PendingToolCall {
    name: String,
    input: serde_json::Value,
}

#[derive(Debug)]
struct PendingToolCallLookup {
    signature: Option<String>,
    diff: Option<String>,
    reason: &'static str,
    tail_bytes: usize,
}

fn one_line_json(value: &serde_json::Value, cap: usize) -> String {
    let mut s = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    s = s.split_whitespace().collect::<Vec<&str>>().join(" ");
    if s.chars().count() > cap {
        let mut truncated = s.chars().take(cap.saturating_sub(1)).collect::<String>();
        truncated.push('…');
        truncated
    } else {
        s
    }
}

fn count_lines_bytes(s: &str) -> (usize, usize) {
    let lines = if s.is_empty() {
        0
    } else {
        s.lines().count().max(1)
    };
    (lines, s.len())
}

// Build a unified-diff string for a pending Edit/MultiEdit tool call so
// the inline permission panel can render it via the existing `<DiffView>`
// XMLUI component (same renderer the worklist item details use at
// Workspace.xmlui:469). Uses `similar::TextDiff::from_lines` so the diff
// rules match the rest of the app's diff surface — the prior hand-rolled
// "first differing line" preview made shared-prefix Edits look like
// no-ops. Refs #170.
fn pending_tool_call_diff<R: tauri::Runtime>(
    call: &PendingToolCall,
    app: &AppHandle<R>,
) -> Option<String> {
    use similar::TextDiff;
    if call.name != "Edit" && call.name != "MultiEdit" {
        return None;
    }
    let input = &call.input;
    let field = |name: &str| input.get(name).and_then(|v| v.as_str()).unwrap_or("");
    let old_string = field("old_string");
    let new_string = field("new_string");
    if old_string.is_empty() && new_string.is_empty() {
        return None;
    }
    // Display path: relativize absolute paths to the project root if
    // possible (matches git's `a/<rel> b/<rel>` convention used elsewhere
    // in Bram's diff surface). If the path isn't under the project root,
    // strip the leading `/` so we don't end up with `a//Users/...`.
    let file_path = field("file_path");
    let display_path = if file_path.is_empty() {
        String::new()
    } else if let Some(root) = project_root(Some(app)) {
        match std::path::Path::new(file_path).strip_prefix(&root) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => file_path.trim_start_matches('/').to_string(),
        }
    } else {
        file_path.trim_start_matches('/').to_string()
    };
    let header_old = if display_path.is_empty() {
        "a".to_string()
    } else {
        format!("a/{}", display_path)
    };
    let header_new = if display_path.is_empty() {
        "b".to_string()
    } else {
        format!("b/{}", display_path)
    };
    let diff_text = TextDiff::from_lines(old_string, new_string)
        .unified_diff()
        .context_radius(3)
        .missing_newline_hint(false)
        .header(&header_old, &header_new)
        .to_string();
    if diff_text.is_empty() {
        None
    } else {
        Some(diff_text)
    }
}

fn patch_paths(patch: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in patch.lines() {
        let candidate = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "))
            .or_else(|| line.strip_prefix("--- a/"))
            .or_else(|| line.strip_prefix("+++ b/"));
        if let Some(path) = candidate {
            let p = path.trim();
            if !p.is_empty() && p != "/dev/null" && !paths.iter().any(|seen| seen == p) {
                paths.push(p.to_string());
            }
        }
    }
    paths
}

fn format_pending_tool_call(call: &PendingToolCall) -> String {
    let input = &call.input;
    let field = |name: &str| input.get(name).and_then(|v| v.as_str()).unwrap_or("");
    match call.name.as_str() {
        "Bash" => format!("Bash({})", field("command")),
        "Edit" | "MultiEdit" => {
            let file_path = field("file_path");
            if file_path.is_empty() {
                format!("{}({})", call.name, one_line_json(input, 200))
            } else {
                // Signature stays a single-line header; the diff content
                // travels in `tool_call_diff` for `<DiffView>` rendering.
                // Refs #170.
                format!("{}({})", call.name, file_path)
            }
        }
        "Write" => {
            let file_path = field("file_path");
            let content = field("content");
            let (lines, bytes) = count_lines_bytes(content);
            if file_path.is_empty() {
                format!("Write(<{} lines, {} bytes>)", lines, bytes)
            } else {
                format!("Write({})\n  <{} lines, {} bytes>", file_path, lines, bytes)
            }
        }
        "Read" => format!("Read({})", field("file_path")),
        "Glob" => format!("Glob({})", field("pattern")),
        "Grep" => {
            let pattern = field("pattern");
            let path = field("path");
            if path.is_empty() {
                format!("Grep({})", pattern)
            } else {
                format!("Grep({} in {})", pattern, path)
            }
        }
        "apply_patch" => {
            let patch = field("patch");
            let paths = patch_paths(patch);
            if paths.is_empty() {
                "apply_patch(<unknown paths>)".to_string()
            } else {
                format!("apply_patch({})", paths.join(", "))
            }
        }
        name if name.starts_with("mcp__") => {
            let parts: Vec<&str> = name.split("__").collect();
            if parts.len() >= 3 {
                format!("{}:{}({})", parts[1], parts[2], one_line_json(input, 200))
            } else {
                format!("{}({})", name, one_line_json(input, 200))
            }
        }
        _ => format!("{}({})", call.name, one_line_json(input, 200)),
    }
}

fn extract_pending_tool_call_from_jsonl(text: &str) -> Option<PendingToolCall> {
    // Two-pass design (replaced the prior reverse-walk-with-breaks because
    // any interleaved regular `user` text turn or thinking-only `assistant`
    // turn between the pending `tool_use` and the search start would
    // prematurely terminate the walk, surfacing as `signature_reason=
    // no-unmatched-tool-use` even when the JSONL clearly had the unresolved
    // entry. Refs #170, repro at 2026-06-02T03:33:47Z where the tool_use
    // line was 6 s old and still missed by the prior parser).
    //
    // Pass 1 collects every `tool_result.tool_use_id` from every `user`
    // message in the buffer — no early termination, no assumption about
    // arrival order. Pass 2 walks lines in reverse and returns the first
    // assistant tool_use whose id isn't in the resolved set.
    let mut parsed: Vec<serde_json::Value> = Vec::new();
    let mut parse_errors: usize = 0;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(value) => parsed.push(value),
            Err(_) => parse_errors += 1,
        }
    }
    let mut resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &parsed {
        if r.get("type").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let Some(arr) = r
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for c in arr {
            if c.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            if let Some(id) = c.get("tool_use_id").and_then(|v| v.as_str()) {
                resolved.insert(id.to_string());
            }
        }
    }
    // Track the last few assistant tool_use IDs we encounter while walking
    // back — surfaced in the dump-on-fail diagnostic so we can see exactly
    // what the parser saw vs. what we expected.
    let mut recent_tool_use_ids: Vec<(String, String, bool)> = Vec::new();
    for r in parsed.iter().rev() {
        if r.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(arr) = r
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for c in arr {
            if c.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if recent_tool_use_ids.len() < 6 {
                recent_tool_use_ids.push((id.to_string(), name.to_string(), resolved.contains(id)));
            }
            if id.is_empty() || resolved.contains(id) {
                continue;
            }
            if name.is_empty() {
                continue;
            }
            let input = c
                .get("input")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            return Some(PendingToolCall {
                name: name.to_string(),
                input,
            });
        }
        // No unresolved tool_use in this assistant message — keep walking
        // back. The latest tool_use might live in a prior assistant turn
        // if interleaved thinking-only or text-only assistant messages
        // came after it (rare, but possible).
    }
    // Dump-on-fail diagnostic: when the parser walks the full buffer and
    // returns None, write a snapshot to /tmp/bram-jsonl-parse-debug.log
    // showing what it actually saw. Pairs with the `[pty-menu]
    // signature_reason=no-unmatched-tool-use` trace to bisect parser-vs-
    // file-state vs. file-state-vs-disk discrepancies. Bounded to a single
    // line per failure; appends. Refs #170.
    // Tally top-level type values across all parsed lines so we can see
    // what shapes the buffer actually contained. Distinguishes "Rust read
    // an empty buffer" from "Rust read fine but skipped every assistant
    // line for some reason."
    let mut type_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut assistant_content_shapes: Vec<String> = Vec::new();
    for r in &parsed {
        let typ = r
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("<no-type>")
            .to_string();
        *type_counts.entry(typ.clone()).or_insert(0) += 1;
        if typ == "assistant" && assistant_content_shapes.len() < 4 {
            // What content types does this assistant message hold?
            let shape = match r
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                Some(arr) => {
                    let ts: Vec<&str> = arr
                        .iter()
                        .map(|c| c.get("type").and_then(|t| t.as_str()).unwrap_or("?"))
                        .collect();
                    format!("[{}]", ts.join(","))
                }
                None => "<no-content-array>".to_string(),
            };
            assistant_content_shapes.push(shape);
        }
    }
    let now = unix_now_ms();
    let snapshot = format!(
        "[{}] [parser-debug] text_bytes={} parsed_lines={} parse_errors={} resolved_ids={} types={:?} assistant_shapes={:?} recent_tool_use_ids={:?}\n",
        format_iso_utc_ms(now),
        text.len(),
        parsed.len(),
        parse_errors,
        resolved.len(),
        type_counts,
        assistant_content_shapes,
        recent_tool_use_ids,
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/bram-jsonl-parse-debug.log")
        .and_then(|mut f| std::io::Write::write_all(&mut f, snapshot.as_bytes()));
    None
}

fn lookup_pending_tool_call<R: tauri::Runtime>(app: &AppHandle<R>) -> PendingToolCallLookup {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = latest_claude_session_path(app).ok().flatten() else {
        return PendingToolCallLookup {
            signature: None,
            diff: None,
            reason: "no-session-path",
            tail_bytes: 0,
        };
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return PendingToolCallLookup {
            signature: None,
            diff: None,
            reason: "open-failed",
            tail_bytes: 0,
        };
    };
    let Ok(metadata) = file.metadata() else {
        return PendingToolCallLookup {
            signature: None,
            diff: None,
            reason: "metadata-failed",
            tail_bytes: 0,
        };
    };
    let file_size = metadata.len();
    // Tail read budget. 256 KB turned out to be too small in practice:
    // Claude's `file-history-snapshot` records (one per file read) are
    // 4-6 KB each, so a session with many Read tool calls pushes the
    // actual assistant/user messages out the back of the window before
    // the parser can see them. Live repro at 2026-06-02T04:19Z showed
    // a 256 KB tail with 40 file-history-snapshot records and zero
    // assistant messages — pending tool_use scrolled past. 4 MB covers
    // ~400 snapshot records plus the usual assistant/user traffic.
    // Cost: a few ms extra of read+parse per menu detection. Refs #170.
    let want: u64 = 4 * 1024 * 1024;
    let read_from = file_size.saturating_sub(want);
    if file.seek(SeekFrom::Start(read_from)).is_err() {
        return PendingToolCallLookup {
            signature: None,
            diff: None,
            reason: "seek-failed",
            tail_bytes: 0,
        };
    }
    let mut tail = Vec::with_capacity((file_size - read_from) as usize);
    if file.read_to_end(&mut tail).is_err() {
        return PendingToolCallLookup {
            signature: None,
            diff: None,
            reason: "read-failed",
            tail_bytes: 0,
        };
    }
    let tail_bytes = tail.len();
    let text = String::from_utf8_lossy(&tail);
    let result = extract_pending_tool_call_from_jsonl(&text);
    // Path + size diagnostic pairs with the parser-debug dump on failure.
    // Written only when the parser returned None so we can correlate.
    if result.is_none() {
        let snapshot = format!(
            "[{}] [parser-debug-lookup] path={:?} file_size={} read_from={} tail_bytes={}\n",
            format_iso_utc_ms(unix_now_ms()),
            path,
            file_size,
            read_from,
            tail_bytes,
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/bram-jsonl-parse-debug.log")
            .and_then(|mut f| std::io::Write::write_all(&mut f, snapshot.as_bytes()));
    }
    match result {
        Some(call) => PendingToolCallLookup {
            signature: Some(format_pending_tool_call(&call)),
            diff: pending_tool_call_diff(&call, app),
            reason: "found",
            tail_bytes,
        },
        None => PendingToolCallLookup {
            signature: None,
            diff: None,
            reason: "no-unmatched-tool-use",
            tail_bytes,
        },
    }
}

// Pick whichever provider's latest session file has the most recent
// mtime. Bypasses `latest_session_path`'s active-agent-hint check —
// the hint is sticky and lags when activity flips between providers,
// causing routes that need live terminal-adjacent state to walk the
// wrong (often empty) session for several refetch cycles.
fn freshest_session_path<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<Option<std::path::PathBuf>, String> {
    let mut best: Option<(std::path::PathBuf, std::time::SystemTime)> = None;
    for path_opt in [
        latest_claude_session_path(app)?,
        latest_codex_session_path(app)?,
    ] {
        let Some(path) = path_opt else { continue };
        let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().map(|(_, t)| mtime > *t).unwrap_or(true) {
            best = Some((path, mtime));
        }
    }
    Ok(best.map(|(p, _)| p))
}

fn read_last_assistant_text<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> Result<Vec<u8>, String> {
    let Some(path) = latest_session_path(app, preferred)? else {
        return Ok(br#"{"text":"","source":"session-turns"}"#.to_vec());
    };
    let metadata = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let turns = st_parse_lines_to_turns(&text);
    let mut found = String::new();
    for turn in turns.iter().rev() {
        if turn.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(text) = turn.get("text").and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    found = text.to_string();
                    break;
                }
            }
        }
    }
    let body = serde_json::json!({
        "text": found,
        "source": "session-turns",
        "path": path.to_string_lossy().to_string(),
        "mtime": modified_ms,
    });
    serde_json::to_vec(&body).map_err(|e| e.to_string())
}

// Companion to read_last_assistant_text: returns the most recent USER
// message text plus the most recent ASSISTANT message text (which may be
// empty if the assistant hasn't responded yet) plus the active provider
// label. Used by the Worklist tab's "You said" / "Claude said" panel.
fn read_last_exchange<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> Result<Vec<u8>, String> {
    let provider = preferred.or_else(|| hinted_session_provider(app));
    let provider_str = match provider {
        Some(SessionProvider::Claude) => "claude",
        Some(SessionProvider::Codex) => "codex",
        None => "",
    };
    let Some(path) = latest_session_path(app, preferred)? else {
        let body = serde_json::json!({
            "userText": "",
            "assistantText": "",
            "tools": [],
            "provider": provider_str,
        });
        return serde_json::to_vec(&body).map_err(|e| e.to_string());
    };
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let turns = st_parse_lines_to_turns(&text);
    let mut user_text = String::new();
    let mut assistant_text = String::new();
    let mut tool_entries: Vec<serde_json::Value> = Vec::new();
    // Walk backward: most recent assistant first, then most recent user
    // *before* that assistant (so they're a pair). If no assistant yet,
    // user_text is the most recent user.
    let mut assistant_idx: Option<usize> = None;
    for (i, turn) in turns.iter().enumerate().rev() {
        if turn.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(t) = turn.get("text").and_then(|v| v.as_str()) {
                if !t.trim().is_empty() {
                    assistant_text = t.to_string();
                    assistant_idx = Some(i);
                    break;
                }
            }
        }
    }
    let user_search_end = assistant_idx.unwrap_or(turns.len());
    let mut user_idx: Option<usize> = None;
    for (i, turn) in turns.iter().enumerate().take(user_search_end).rev() {
        if turn.get("role").and_then(|v| v.as_str()) == Some("user") {
            if let Some(t) = turn.get("text").and_then(|v| v.as_str()) {
                if !t.trim().is_empty() {
                    user_text = t.to_string();
                    user_idx = Some(i);
                    break;
                }
            }
        }
    }
    if let Some(start_idx) = user_idx {
        let end_idx = assistant_idx.unwrap_or(turns.len().saturating_sub(1));
        for turn in turns
            .iter()
            .take(end_idx.saturating_add(1))
            .skip(start_idx + 1)
        {
            let Some(entries) = turn.get("entries").and_then(|v| v.as_array()) else {
                continue;
            };
            for entry in entries {
                if entry.get("kind").and_then(|v| v.as_str()) == Some("tool") {
                    tool_entries.push(entry.clone());
                }
            }
        }
    }
    if tool_entries.len() > 5 {
        let keep_from = tool_entries.len() - 5;
        tool_entries.drain(0..keep_from);
    }
    let body = serde_json::json!({
        "userText": user_text,
        "assistantText": assistant_text,
        "tools": tool_entries,
        "provider": provider_str,
    });
    serde_json::to_vec(&body).map_err(|e| e.to_string())
}

// Host-side `is the agent waiting for the assistant to speak` derivation.
// Mirrors the iframe helper isWaitingForAssistant(jsonlText) in Globals.xs:
// returns true when the most recent meaningful record is a user message
// (tool_result-only user records are skipped). Used by the Transcript
// tab's "agent is thinking" spinner and the TextArea `enabled` binding.
fn read_waiting_for_assistant<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = freshest_session_path(app)? else {
        return Ok(br#"{"waiting":false}"#.to_vec());
    };
    let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let file_size = file.metadata().map_err(|e| e.to_string())?.len();
    // Last 50 records typically fit in 32 KB even on heavy turns.
    let want: u64 = 32 * 1024;
    let read_from = file_size.saturating_sub(want);
    file.seek(SeekFrom::Start(read_from))
        .map_err(|e| e.to_string())?;
    let mut tail = Vec::with_capacity((file_size - read_from) as usize);
    file.read_to_end(&mut tail).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&tail);
    let mut last_role: Option<&str> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let typ = r.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if typ == "user" {
            let Some(content) = r.get("message").and_then(|m| m.get("content")) else {
                continue;
            };
            if let Some(arr) = content.as_array() {
                let all_tool_result = !arr.is_empty()
                    && arr
                        .iter()
                        .all(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
                if all_tool_result {
                    continue;
                }
            }
            last_role = Some("user");
        } else if typ == "assistant" {
            let Some(content) = r.get("message").and_then(|m| m.get("content")) else {
                continue;
            };
            let has_text = content.as_str().map(|s| !s.is_empty()).unwrap_or(false)
                || content
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .any(|c| c.get("type").and_then(|t| t.as_str()) == Some("text"))
                    })
                    .unwrap_or(false);
            if has_text {
                last_role = Some("assistant");
            }
        } else if typ == "event_msg" {
            if let Some(payload) = r.get("payload") {
                match payload.get("type").and_then(|v| v.as_str()) {
                    Some("user_message") => last_role = Some("user"),
                    Some("agent_message") => last_role = Some("assistant"),
                    _ => {}
                }
            }
        } else if typ == "response_item" {
            if let Some(payload) = r.get("payload") {
                if payload.get("type").and_then(|v| v.as_str()) == Some("message") {
                    let has_text = payload
                        .get("content")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter().any(|c| {
                                let c_typ = c.get("type").and_then(|v| v.as_str());
                                let has_text = c
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .map(|t| !t.is_empty())
                                    .unwrap_or(false);
                                matches!(
                                    c_typ,
                                    Some("input_text") | Some("output_text") | Some("text")
                                ) && has_text
                            })
                        })
                        .unwrap_or(false);
                    if has_text {
                        match payload.get("role").and_then(|v| v.as_str()) {
                            Some("user") => last_role = Some("user"),
                            Some("assistant") => last_role = Some("assistant"),
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    let waiting = last_role == Some("user");
    let body = serde_json::json!({ "waiting": waiting });
    serde_json::to_vec(&body).map_err(|e| e.to_string())
}

// Host-side current-turn edits extraction. Mirrors the iframe helper
// `currentTurnEdits(jsonlText)` in Globals.xs: walks backward to find
// the most recent user-message boundary, then collects per-file
// aggregates (kind, added/removed line counts, lastToolId) from
// Claude tool_use blocks and Codex apply_patch payloads after that
// boundary.
//
// Returns a JSON array of {filePath, kind, added, removed, lastToolId}
// in first-touch order. Empty array when there are no edits in the
// current turn or no session.
fn read_current_turn_edits<R: tauri::Runtime>(
    app: &AppHandle<R>,
    _preferred: Option<SessionProvider>,
) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = freshest_session_path(app)? else {
        return Ok(b"[]".to_vec());
    };
    let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let file_size = file.metadata().map_err(|e| e.to_string())?.len();
    // 64 KB tail covers ~100 records comfortably even with Codex's
    // verbose apply_patch payloads. Bigger than read_last_assistant_text's
    // 32 KB because patch records can be heavy.
    let want: u64 = 64 * 1024;
    let read_from = file_size.saturating_sub(want);
    file.seek(SeekFrom::Start(read_from))
        .map_err(|e| e.to_string())?;
    let mut tail = Vec::with_capacity((file_size - read_from) as usize);
    file.read_to_end(&mut tail).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&tail);
    let lines: Vec<&str> = text.lines().collect();

    // Walk backward to the most recent user-message boundary.
    // tool_result-only Claude user records don't count as the boundary
    // (they're tool outputs, not actual user messages).
    let mut last_user_idx: Option<usize> = None;
    for i in (0..lines.len()).rev() {
        let line = lines[i].trim();
        if line.is_empty() {
            continue;
        }
        let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let typ = r.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if typ == "user" {
            if let Some(arr) = r
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                let all_tool_result = !arr.is_empty()
                    && arr
                        .iter()
                        .all(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
                if all_tool_result {
                    continue;
                }
            }
            last_user_idx = Some(i);
            break;
        }
        if typ == "event_msg" {
            if let Some(p) = r.get("payload") {
                if p.get("type").and_then(|v| v.as_str()) == Some("user_message") {
                    last_user_idx = Some(i);
                    break;
                }
            }
        }
    }

    struct Bucket {
        kind: Option<&'static str>,
        added: u64,
        removed: u64,
        last_tool_id: Option<String>,
    }
    let mut by_file: std::collections::HashMap<String, Bucket> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();

    fn merge_kind(prev: Option<&'static str>, new_kind: &'static str) -> &'static str {
        match prev {
            None => new_kind,
            Some(p) if p == new_kind => p,
            _ => "mixed",
        }
    }

    fn ensure<'a>(
        by_file: &'a mut std::collections::HashMap<String, Bucket>,
        order: &mut Vec<String>,
        path: &str,
    ) -> &'a mut Bucket {
        if !by_file.contains_key(path) {
            by_file.insert(
                path.to_string(),
                Bucket {
                    kind: None,
                    added: 0,
                    removed: 0,
                    last_tool_id: None,
                },
            );
            order.push(path.to_string());
        }
        by_file.get_mut(path).unwrap()
    }

    let start = last_user_idx.map(|i| i + 1).unwrap_or(0);
    for i in start..lines.len() {
        let line = lines[i].trim();
        if line.is_empty() {
            continue;
        }
        let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let typ = r.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Codex: response_item with function_call/custom_tool_call,
        // name == apply_patch, arguments carrying a patch text.
        if typ == "response_item" {
            let Some(p) = r.get("payload") else { continue };
            let ptype = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ptype != "function_call" && ptype != "custom_tool_call" {
                continue;
            }
            if p.get("name").and_then(|v| v.as_str()) != Some("apply_patch") {
                continue;
            }
            // arguments is typically a JSON string {"input": "<patch text>"}.
            // Fall back to direct field access if the shape differs.
            let patch_text: String = match p.get("arguments") {
                Some(serde_json::Value::String(s)) => serde_json::from_str::<serde_json::Value>(s)
                    .ok()
                    .and_then(|v| v.get("input").and_then(|i| i.as_str()).map(String::from))
                    .unwrap_or_default(),
                Some(v) => v
                    .get("input")
                    .and_then(|i| i.as_str())
                    .map(String::from)
                    .unwrap_or_default(),
                None => String::new(),
            };
            if patch_text.is_empty() {
                continue;
            }
            let call_id = p.get("call_id").and_then(|v| v.as_str()).map(String::from);
            let mut current_path: Option<String> = None;
            for pl in patch_text.lines() {
                if let Some(rest) = pl.strip_prefix("*** ") {
                    let mut new_kind: Option<&'static str> = None;
                    let mut new_path: Option<String> = None;
                    if let Some(p) = rest.strip_prefix("Add File: ") {
                        new_kind = Some("added");
                        new_path = Some(p.trim().to_string());
                    } else if let Some(p) = rest.strip_prefix("Update File: ") {
                        new_kind = Some("updated");
                        new_path = Some(p.trim().to_string());
                    } else if let Some(p) = rest.strip_prefix("Delete File: ") {
                        new_kind = Some("deleted");
                        new_path = Some(p.trim().to_string());
                    }
                    if let (Some(kind), Some(path)) = (new_kind, new_path) {
                        let bucket = ensure(&mut by_file, &mut order, &path);
                        bucket.kind = Some(merge_kind(bucket.kind, kind));
                        if let Some(id) = &call_id {
                            bucket.last_tool_id = Some(id.clone());
                        }
                        current_path = Some(path);
                    } else {
                        current_path = None;
                    }
                    continue;
                }
                let Some(path) = current_path.as_ref() else {
                    continue;
                };
                if pl.starts_with("+++") || pl.starts_with("---") {
                    continue;
                }
                if let Some(bucket) = by_file.get_mut(path) {
                    if pl.starts_with('+') {
                        bucket.added += 1;
                    } else if pl.starts_with('-') {
                        bucket.removed += 1;
                    }
                }
            }
            continue;
        }

        // Claude: assistant.message.content[*] with type=tool_use,
        // name in {Edit, MultiEdit, Write}.
        if typ != "assistant" {
            continue;
        }
        let Some(content_arr) = r
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for c in content_arr {
            if c.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name != "Edit" && name != "MultiEdit" && name != "Write" {
                continue;
            }
            let Some(input) = c.get("input") else {
                continue;
            };
            let file_path = match input.get("file_path").and_then(|p| p.as_str()) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => continue,
            };
            let call_id = c.get("id").and_then(|v| v.as_str()).map(String::from);
            let bucket = ensure(&mut by_file, &mut order, &file_path);
            if let Some(id) = &call_id {
                bucket.last_tool_id = Some(id.clone());
            }
            match name {
                "Edit" => {
                    let before = input
                        .get("old_string")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let after = input
                        .get("new_string")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !before.is_empty() {
                        bucket.removed += before.lines().count() as u64;
                    }
                    if !after.is_empty() {
                        bucket.added += after.lines().count() as u64;
                    }
                    bucket.kind = Some(merge_kind(bucket.kind, "edited"));
                }
                "MultiEdit" => {
                    if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
                        for e in edits {
                            let before = e.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
                            let after = e.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                            if !before.is_empty() {
                                bucket.removed += before.lines().count() as u64;
                            }
                            if !after.is_empty() {
                                bucket.added += after.lines().count() as u64;
                            }
                        }
                    }
                    bucket.kind = Some(merge_kind(bucket.kind, "multi-edited"));
                }
                "Write" => {
                    let after = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    if !after.is_empty() {
                        bucket.added += after.lines().count() as u64;
                    }
                    bucket.kind = Some(merge_kind(bucket.kind, "written"));
                }
                _ => {}
            }
        }
    }

    let result: Vec<serde_json::Value> = order
        .iter()
        .filter_map(|fp| {
            by_file.remove(fp).map(|b| {
                serde_json::json!({
                    "filePath": fp,
                    "kind": b.kind,
                    "added": b.added,
                    "removed": b.removed,
                    "lastToolId": b.last_tool_id,
                })
            })
        })
        .collect();
    serde_json::to_vec(&result).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// Session-turns port: replaces the iframe sessionTurns / getToolDetail
// helper chain (~400 LOC of JSONL walking and shape-massaging) with a
// host-side derivation that emits the same structured array Transcript
// renders against today. Pure functions first, then the route entry
// points read_session_turns / read_tool_detail.
//
// Output shape (must match the iframe so Transcript.xmlui doesn't
// need to change other than its DataSource binding):
//   [{
//     role: "user" | "assistant",
//     text: <joined string of text entries>,
//     entries: [
//       { kind: "text", text },
//       { kind: "tool", id, name, summary, errored?, errorText? }
//     ],
//     images: [<inline base64 data: URLs OR extracted image paths>]
//   }, ...]
// ---------------------------------------------------------------------

fn st_strip_image_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find("[Image: source: /") else {
            out.push_str(rest);
            break;
        };
        // Find the end of the bracketed marker (next ']').
        let after_start = &rest[start..];
        let Some(close) = after_start.find(']') else {
            out.push_str(rest);
            break;
        };
        // The iframe's regex matches `[Image: source: /<path>.(png|jpg|jpeg|gif|webp)]`
        // plus the leading `\n*`. We try the bracket-only match and
        // verify it's an image extension; if not, treat as literal text.
        let marker = &after_start[..=close];
        let dot = marker.rfind('.');
        let ext_ok = dot
            .map(|d| {
                let ext = &marker[d + 1..marker.len() - 1].to_ascii_lowercase();
                matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
            })
            .unwrap_or(false);
        if !ext_ok {
            // Not an image marker — emit through start+1 to skip past
            // the `[` and continue scanning.
            out.push_str(&rest[..start + 1]);
            rest = &rest[start + 1..];
            continue;
        }
        // Strip preceding newlines (any number).
        let mut prefix_end = start;
        while prefix_end > 0 && rest.as_bytes()[prefix_end - 1] == b'\n' {
            prefix_end -= 1;
        }
        out.push_str(&rest[..prefix_end]);
        rest = &rest[start + close + 1..];
    }
    out
}

fn st_extract_image_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[Image: source: /") {
        let after = &rest[start..];
        let Some(close) = after.find(']') else { break };
        let marker = &after[..=close];
        // Strip prefix "[Image: source: " and suffix "]" to get the path.
        let path = &marker["[Image: source: ".len()..marker.len() - 1];
        let lower_path = path.to_ascii_lowercase();
        if lower_path.ends_with(".png")
            || lower_path.ends_with(".jpg")
            || lower_path.ends_with(".jpeg")
            || lower_path.ends_with(".gif")
            || lower_path.ends_with(".webp")
        {
            paths.push(path.to_string());
        }
        rest = &rest[start + close + 1..];
    }
    paths
}

fn st_rewrite_xmlui_doc_urls(text: &str) -> String {
    text.replace(
        "https://docs.xmlui.org/components/",
        "https://www.xmlui.org/docs/reference/components/",
    )
    .replace("https://docs.xmlui.org/", "https://www.xmlui.org/docs/")
}

fn st_tool_result_text(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let mut parts: Vec<String> = Vec::new();
        for c in arr {
            if c.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                    parts.push(t.to_string());
                }
            }
        }
        return parts.join("\n");
    }
    String::new()
}

fn st_is_error_result(block: &serde_json::Value) -> bool {
    if block
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    let content = block
        .get("content")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let text = st_tool_result_text(&content);
    text.starts_with("Error:") || text.starts_with("<tool_use_error>")
}

fn st_extract_lines(text: &str, cap: usize) -> Option<serde_json::Value> {
    if text.is_empty() {
        return None;
    }
    let all: Vec<&str> = text.split('\n').collect();
    let lines: Vec<&str> = all.iter().take(cap).copied().collect();
    let remaining = all.len().saturating_sub(cap);
    Some(serde_json::json!({
        "lines": lines,
        "remaining": remaining,
    }))
}

fn st_extract_tool_result(block: &serde_json::Value, cap: usize) -> serde_json::Value {
    let content = block
        .get("content")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let text = st_tool_result_text(&content);
    st_extract_lines(&text, cap).unwrap_or(serde_json::Value::Null)
}

fn st_tool_summary(name: &str, input: &serde_json::Value) -> String {
    let obj = match input.as_object() {
        Some(o) => o,
        None => return name.to_string(),
    };
    let get_str = |k: &str| -> &str { obj.get(k).and_then(|v| v.as_str()).unwrap_or("") };
    match name {
        "Edit" | "MultiEdit" => format!("{} edited", get_str("file_path")),
        "Write" => {
            let content = get_str("content");
            let lines = if content.is_empty() {
                1
            } else {
                content.split('\n').count()
            };
            format!(
                "{} — wrote {} line{}",
                get_str("file_path"),
                lines,
                if lines == 1 { "" } else { "s" }
            )
        }
        "Bash" => {
            let cmd = get_str("command");
            if cmd.chars().count() > 80 {
                let truncated: String = cmd.chars().take(80).collect();
                format!("{}…", truncated)
            } else {
                cmd.to_string()
            }
        }
        "Read" => {
            let mut s = get_str("file_path").to_string();
            let offset = obj.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
            let limit = obj.get("limit").and_then(|v| v.as_u64()).unwrap_or(0);
            if offset > 0 || limit > 0 {
                let start = if offset > 0 { offset } else { 1 };
                s.push(':');
                s.push_str(&start.to_string());
                if limit > 0 {
                    s.push('-');
                    s.push_str(&(start + limit - 1).to_string());
                }
            }
            s
        }
        "Grep" | "Glob" => {
            let pattern = get_str("pattern");
            let path = get_str("path");
            if path.is_empty() {
                pattern.to_string()
            } else {
                format!("{} in {}", pattern, path)
            }
        }
        "Task" | "Agent" => {
            let typ = get_str("subagent_type");
            let desc = get_str("description");
            if desc.is_empty() {
                typ.to_string()
            } else {
                format!("{} — {}", typ, desc)
            }
        }
        _ => name.to_string(),
    }
}

fn st_codex_tool_name(payload: &serde_json::Value) -> String {
    let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(ns) = payload.get("namespace").and_then(|v| v.as_str()) {
        let stripped = ns.strip_prefix("mcp__").unwrap_or(ns);
        format!("{}.{}", stripped, name)
    } else {
        name.to_string()
    }
}

fn st_parse_json_string(s: &str) -> Option<serde_json::Value> {
    serde_json::from_str(s).ok()
}

fn st_codex_tool_input(payload: &serde_json::Value) -> serde_json::Value {
    let typ = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if typ == "function_call" {
        if let Some(args) = payload.get("arguments") {
            if let Some(s) = args.as_str() {
                return st_parse_json_string(s)
                    .unwrap_or_else(|| serde_json::Value::String(s.to_string()));
            }
            return args.clone();
        }
        return serde_json::json!({});
    }
    if typ == "custom_tool_call" {
        if let Some(inp) = payload.get("input") {
            if let Some(s) = inp.as_str() {
                return st_parse_json_string(s)
                    .unwrap_or_else(|| serde_json::Value::String(s.to_string()));
            }
            return inp.clone();
        }
        return serde_json::Value::String(String::new());
    }
    serde_json::json!({})
}

fn st_codex_tool_summary(payload: &serde_json::Value) -> String {
    let raw_name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let full_name = st_codex_tool_name(payload);
    let input = st_codex_tool_input(payload);
    if raw_name == "exec_command" {
        if let Some(cmd) = input.get("cmd").and_then(|v| v.as_str()) {
            if cmd.chars().count() > 80 {
                let truncated: String = cmd.chars().take(80).collect();
                return format!("{}…", truncated);
            }
            return cmd.to_string();
        }
    }
    if raw_name == "write_stdin" {
        let session = input
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| format!("session {}", s))
            .unwrap_or_else(|| "stdin".to_string());
        let chars = input.get("chars").and_then(|v| v.as_str()).unwrap_or("");
        if chars.is_empty() {
            return session;
        }
        let label = if chars == "\u{001b}" {
            "Esc".to_string()
        } else {
            chars.replace('\r', "\\r").replace('\n', "\\n")
        };
        let label_clipped = if label.chars().count() > 40 {
            let t: String = label.chars().take(40).collect();
            format!("{}…", t)
        } else {
            label
        };
        return format!("{} ← {}", session, label_clipped);
    }
    if raw_name == "apply_patch" {
        if let Some(s) = input.as_str() {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("*** Add File: ") {
                    return format!("{} patch", rest);
                }
                if let Some(rest) = line.strip_prefix("*** Update File: ") {
                    return format!("{} patch", rest);
                }
                if let Some(rest) = line.strip_prefix("*** Delete File: ") {
                    return format!("{} patch", rest);
                }
            }
            return "patch".to_string();
        }
    }
    if full_name.starts_with("filesystem.") {
        if let Some(p) = input.get("path").and_then(|v| v.as_str()) {
            return p.to_string();
        }
    }
    if full_name.starts_with("xmlui.") {
        for k in &["path", "component", "query"] {
            if let Some(v) = input.get(*k).and_then(|v| v.as_str()) {
                return v.to_string();
            }
        }
        return full_name;
    }
    if input.is_object() {
        return st_tool_summary(raw_name, &input);
    }
    full_name
}

fn st_codex_tool_output(payload: &serde_json::Value) -> Option<(String, bool)> {
    let typ = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if typ != "function_call_output" && typ != "custom_tool_call_output" {
        return None;
    }
    let raw = match payload.get("output").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return Some((String::new(), false)),
    };
    if let Some(parsed) = st_parse_json_string(raw) {
        if parsed.is_object() {
            let text = parsed
                .get("output")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| {
                    parsed
                        .get("stderr")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
                .unwrap_or_else(|| raw.to_string());
            let exit_code = parsed
                .get("metadata")
                .and_then(|m| m.get("exit_code"))
                .and_then(|v| v.as_i64());
            let errored = matches!(exit_code, Some(n) if n != 0);
            return Some((text, errored));
        }
    }
    // Fallback: look for the "Process exited with code N" pattern.
    let errored = raw
        .lines()
        .find_map(|l| l.strip_prefix("Process exited with code "))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<i64>().ok())
        .map(|n| n != 0)
        .unwrap_or(false);
    Some((raw.to_string(), errored))
}

// Walk JSONL lines and build the structured turn array. Mirrors
// `_parseLinesToTurns` in Globals.xs.
fn st_parse_lines_to_turns(jsonl_text: &str) -> Vec<serde_json::Value> {
    let mut turns: Vec<serde_json::Value> = Vec::new();
    // tool_entry_locations maps tool_use_id → (turn_idx, entry_idx) so
    // tool_result records can backfill errored/errorText on the
    // originating entry.
    let mut tool_entry_locations: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();
    for line in jsonl_text.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let typ = r.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mut role: Option<&str> = None;
        let mut entries: Vec<serde_json::Value> = Vec::new();
        let mut inline_images: Vec<String> = Vec::new();

        if typ == "user" || typ == "assistant" {
            let Some(content) = r.get("message").and_then(|m| m.get("content")) else {
                continue;
            };
            role = Some(typ);
            if let Some(s) = content.as_str() {
                if !s.is_empty() {
                    entries.push(serde_json::json!({ "kind": "text", "text": s }));
                }
            } else if let Some(arr) = content.as_array() {
                for c in arr {
                    let Some(c_obj) = c.as_object() else { continue };
                    let c_typ = c_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if c_typ == "text" {
                        if let Some(t) = c_obj.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                entries.push(serde_json::json!({ "kind": "text", "text": t }));
                            }
                        }
                    } else if c_typ == "tool_use" {
                        let id = c_obj.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let name = c_obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let empty = serde_json::json!({});
                        let input = c_obj.get("input").unwrap_or(&empty);
                        let summary = st_tool_summary(name, input);
                        let entry = serde_json::json!({
                            "kind": "tool",
                            "id": id,
                            "name": name,
                            "summary": summary,
                        });
                        let entry_idx = entries.len();
                        entries.push(entry);
                        if !id.is_empty() {
                            // Will fix the turn index after we push the turn.
                            tool_entry_locations.insert(id.to_string(), (turns.len(), entry_idx));
                        }
                    } else if c_typ == "tool_result" {
                        let tool_use_id = c_obj
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if let Some((turn_idx, entry_idx)) =
                            tool_entry_locations.get(tool_use_id).copied()
                        {
                            if st_is_error_result(c) {
                                let txt = st_tool_result_text(
                                    c.get("content").unwrap_or(&serde_json::Value::Null),
                                );
                                let first_line = txt
                                    .split('\n')
                                    .next()
                                    .unwrap_or("")
                                    .chars()
                                    .take(200)
                                    .collect::<String>();
                                if let Some(turn_obj) =
                                    turns.get_mut(turn_idx).and_then(|t| t.as_object_mut())
                                {
                                    if let Some(entries_arr) =
                                        turn_obj.get_mut("entries").and_then(|e| e.as_array_mut())
                                    {
                                        if let Some(entry_obj) = entries_arr
                                            .get_mut(entry_idx)
                                            .and_then(|e| e.as_object_mut())
                                        {
                                            entry_obj.insert(
                                                "errored".to_string(),
                                                serde_json::Value::Bool(true),
                                            );
                                            entry_obj.insert(
                                                "errorText".to_string(),
                                                serde_json::Value::String(first_line),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    } else if c_typ == "image" {
                        if let Some(source) = c_obj.get("source").and_then(|v| v.as_object()) {
                            if source.get("type").and_then(|v| v.as_str()) == Some("base64") {
                                if let Some(data) = source.get("data").and_then(|v| v.as_str()) {
                                    let mt = source
                                        .get("media_type")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("image/png");
                                    inline_images.push(format!("data:{};base64,{}", mt, data));
                                }
                            }
                        }
                    }
                }
            }
        } else if typ == "event_msg" {
            if let Some(p) = r.get("payload") {
                let p_typ = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if p_typ == "user_message" {
                    role = Some("user");
                } else if p_typ == "agent_message" {
                    role = Some("assistant");
                }
                if let Some(msg) = p.get("message").and_then(|v| v.as_str()) {
                    if !msg.is_empty() {
                        entries.push(serde_json::json!({ "kind": "text", "text": msg }));
                    }
                }
            }
        } else if typ == "response_item" {
            if let Some(p) = r.get("payload") {
                let p_typ = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if p_typ == "message" {
                    match p.get("role").and_then(|v| v.as_str()) {
                        Some("user") => role = Some("user"),
                        Some("assistant") => role = Some("assistant"),
                        _ => {}
                    }
                    if let Some(arr) = p.get("content").and_then(|v| v.as_array()) {
                        for c in arr {
                            let c_typ = c.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if c_typ == "input_text" || c_typ == "output_text" || c_typ == "text" {
                                if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
                                    if !t.is_empty() {
                                        entries
                                            .push(serde_json::json!({ "kind": "text", "text": t }));
                                    }
                                }
                            }
                        }
                    }
                } else if p_typ == "function_call" || p_typ == "custom_tool_call" {
                    role = Some("assistant");
                    let id = p.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = st_codex_tool_name(p);
                    let summary = st_codex_tool_summary(p);
                    let entry = serde_json::json!({
                        "kind": "tool",
                        "id": id,
                        "name": name,
                        "summary": summary,
                    });
                    let entry_idx = entries.len();
                    entries.push(entry);
                    if !id.is_empty() {
                        tool_entry_locations.insert(id.to_string(), (turns.len(), entry_idx));
                    }
                } else if p_typ == "function_call_output" || p_typ == "custom_tool_call_output" {
                    let id = p.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some((turn_idx, entry_idx)) = tool_entry_locations.get(id).copied() {
                        if let Some((text, errored)) = st_codex_tool_output(p) {
                            if errored {
                                let first_line = text
                                    .split('\n')
                                    .next()
                                    .unwrap_or("")
                                    .chars()
                                    .take(200)
                                    .collect::<String>();
                                if let Some(turn_obj) =
                                    turns.get_mut(turn_idx).and_then(|t| t.as_object_mut())
                                {
                                    if let Some(entries_arr) =
                                        turn_obj.get_mut("entries").and_then(|e| e.as_array_mut())
                                    {
                                        if let Some(entry_obj) = entries_arr
                                            .get_mut(entry_idx)
                                            .and_then(|e| e.as_object_mut())
                                        {
                                            entry_obj.insert(
                                                "errored".to_string(),
                                                serde_json::Value::Bool(true),
                                            );
                                            entry_obj.insert(
                                                "errorText".to_string(),
                                                serde_json::Value::String(first_line),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let Some(role) = role else { continue };
        if entries.is_empty() && inline_images.is_empty() {
            continue;
        }

        // Capture image paths from the ORIGINAL text BEFORE stripping.
        let original_joined = entries
            .iter()
            .filter_map(|e| {
                if e.get("kind").and_then(|k| k.as_str()) == Some("text") {
                    e.get("text").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let paths_from_text = st_extract_image_paths(&original_joined);

        // Apply text rewrites + strip image-path footers.
        for e in entries.iter_mut() {
            if e.get("kind").and_then(|k| k.as_str()) == Some("text") {
                if let Some(t) = e.get("text").and_then(|v| v.as_str()) {
                    let rewritten = st_rewrite_xmlui_doc_urls(t);
                    let stripped = st_strip_image_paths(&rewritten);
                    if let Some(obj) = e.as_object_mut() {
                        obj.insert("text".to_string(), serde_json::Value::String(stripped));
                    }
                }
            }
        }

        // Skip user turns that are pure image-path bookkeeping.
        if role == "user" && inline_images.is_empty() {
            let all_text = entries
                .iter()
                .all(|e| e.get("kind").and_then(|k| k.as_str()) == Some("text"));
            let original_trimmed = original_joined.trim();
            let mut is_image_only = !original_trimmed.is_empty();
            for chunk in original_trimmed.split_whitespace() {
                if !(chunk.starts_with("[Image:") && chunk.ends_with("]")) {
                    is_image_only = false;
                    break;
                }
            }
            // The iframe's regex is more permissive about whitespace; replicate by
            // checking that what remains after stripping image markers is empty.
            let stripped_check = st_strip_image_paths(original_trimmed);
            if all_text
                && (is_image_only || stripped_check.trim().is_empty())
                && !paths_from_text.is_empty()
            {
                continue;
            }
        }

        // After tool_result filtering, may be empty.
        if entries.is_empty() && inline_images.is_empty() {
            continue;
        }

        let text_joined = entries
            .iter()
            .filter_map(|e| {
                if e.get("kind").and_then(|k| k.as_str()) == Some("text") {
                    e.get("text").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let images: Vec<String> = if !inline_images.is_empty() {
            inline_images
        } else {
            paths_from_text
        };

        turns.push(serde_json::json!({
            "role": role,
            "text": text_joined,
            "entries": entries,
            "images": images,
        }));
    }
    turns
}

// Single-entry mtime cache for read_session_turns. The parse runs
// ~300 ms on a 600+ turn session; in steady-state polling (every ~2 s
// from `currentTurnEditsTick`) the JSONL mtime is unchanged across
// most fetches, so the cache hit drops the route to a stat() call.
// When the agent appends, mtime advances and we re-parse once.
// Path is part of the key so a provider flip (Claude ↔ Codex) misses
// the cache cleanly and reparses against the new file.
static SESSION_TURNS_CACHE: std::sync::Mutex<
    Option<(std::path::PathBuf, std::time::SystemTime, Vec<u8>)>,
> = std::sync::Mutex::new(None);

// Read the freshest session JSONL and produce the structured turn array.
// Session-size thresholds. Calibrated from observation: at ~21 MB the
// agent pane became unusable; at ~5 MB earlier in the same session
// everything was responsive. The middle band is a guess and worth
// tightening once we have more data.
const SESSION_SIZE_GREEN_MAX_BYTES: u64 = 5 * 1024 * 1024;
const SESSION_SIZE_AMBER_MAX_BYTES: u64 = 15 * 1024 * 1024;

fn format_bytes_human(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn read_session_size<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<Vec<u8>, String> {
    let Some(path) = freshest_session_path(app)? else {
        return Ok(b"{}".to_vec());
    };
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let human_bytes = format_bytes_human(bytes);
    let state = if bytes <= SESSION_SIZE_GREEN_MAX_BYTES {
        "green"
    } else if bytes <= SESSION_SIZE_AMBER_MAX_BYTES {
        "amber"
    } else {
        "red"
    };
    let provider_label = match hinted_session_provider(app) {
        Some(SessionProvider::Codex) => "codex",
        Some(SessionProvider::Claude) => "claude",
        None => "unknown",
    };
    // Per-agent reset guidance. Both Claude and Codex re-attach to the
    // current session JSONL by default when the wrapper script
    // remembers a session id; the user-facing fix is the same shape
    // for both — exit the current agent process and re-launch without
    // the resume flag — but the exact command differs by agent. Keep
    // the wording calm and the command copy-pasteable.
    let (reset_command, guidance) = match (provider_label, state) {
        ("claude", "amber") => (
            "claude",
            format!(
                "Session is {} (warming up). Approaching iframe slowdown — \
                 consider starting fresh: exit the agent (Ctrl-C twice) and \
                 run `claude` in the terminal.",
                human_bytes
            ),
        ),
        ("claude", "red") => (
            "claude",
            format!(
                "Session is {} (over 15 MB). Recommend starting fresh: exit \
                 the agent (Ctrl-C twice) and run `claude` in the terminal.",
                human_bytes
            ),
        ),
        ("codex", "amber") => (
            "codex",
            format!(
                "Session is {} (warming up). Approaching iframe slowdown — \
                 consider starting fresh: exit the agent (Ctrl-C twice) and \
                 run `codex` in the terminal.",
                human_bytes
            ),
        ),
        ("codex", "red") => (
            "codex",
            format!(
                "Session is {} (over 15 MB). Recommend starting fresh: exit \
                 the agent (Ctrl-C twice) and run `codex` in the terminal.",
                human_bytes
            ),
        ),
        _ => ("", String::new()),
    };
    let body = serde_json::json!({
        "bytes": bytes,
        "humanBytes": human_bytes,
        "state": state,
        "provider": provider_label,
        "resetCommand": reset_command,
        "guidance": guidance,
        "thresholds": {
            "greenMaxBytes": SESSION_SIZE_GREEN_MAX_BYTES,
            "amberMaxBytes": SESSION_SIZE_AMBER_MAX_BYTES,
        }
    });
    serde_json::to_vec(&body).map_err(|e| e.to_string())
}

fn session_size_status_row<R: tauri::Runtime>(app: &AppHandle<R>) -> serde_json::Value {
    let body = read_session_size(app)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let state = body
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let human_bytes = body
        .get("humanBytes")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let provider = body
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let guidance = body.get("guidance").and_then(|v| v.as_str()).unwrap_or("");
    serde_json::json!({
        "signal": "Session size",
        "level": match state {
            "red" | "amber" => "warn",
            "green" => "ok",
            _ => "neutral",
        },
        "state": format!("{} {}", human_bytes, state),
        "detail": if guidance.is_empty() {
            format!("Active {} session is {}", provider, human_bytes)
        } else {
            guidance.to_string()
        },
        "seen": format_iso_utc_ms(unix_now_ms()),
    })
}

fn read_session_turns<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<Vec<u8>, String> {
    let Some(path) = freshest_session_path(app)? else {
        return Ok(b"[]".to_vec());
    };
    let mtime = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map_err(|e| e.to_string())?;
    // Cache hit: same path + same mtime as last serve.
    if let Ok(guard) = SESSION_TURNS_CACHE.lock() {
        if let Some((cached_path, cached_mtime, cached_bytes)) = guard.as_ref() {
            if cached_path == &path && cached_mtime == &mtime {
                return Ok(cached_bytes.clone());
            }
        }
    }
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let turns = st_parse_lines_to_turns(&text);
    let bytes = serde_json::to_vec(&turns).map_err(|e| e.to_string())?;
    if let Ok(mut guard) = SESSION_TURNS_CACHE.lock() {
        *guard = Some((path, mtime, bytes.clone()));
    }
    Ok(bytes)
}

// Single-tool lookup: scan all JSONL records for the tool_use (or codex
// function_call) by id, plus its matching tool_result, return
// { input, result }. result is { lines, remaining } or null.
fn read_tool_detail<R: tauri::Runtime>(
    app: &AppHandle<R>,
    tool_id: &str,
) -> Result<Vec<u8>, String> {
    if tool_id.is_empty() {
        return Ok(b"null".to_vec());
    }
    let Some(path) = freshest_session_path(app)? else {
        return Ok(b"null".to_vec());
    };
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut input: Option<serde_json::Value> = None;
    let mut result: Option<serde_json::Value> = None;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(arr) = r
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            for c in arr {
                let c_typ = c.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if c_typ == "tool_use" && c.get("id").and_then(|v| v.as_str()) == Some(tool_id) {
                    input = Some(
                        c.get("input")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({})),
                    );
                } else if c_typ == "tool_result"
                    && c.get("tool_use_id").and_then(|v| v.as_str()) == Some(tool_id)
                {
                    result = Some(st_extract_tool_result(c, 20));
                }
            }
        } else if r.get("type").and_then(|v| v.as_str()) == Some("response_item") {
            if let Some(p) = r.get("payload") {
                let p_typ = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let call_id = p.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                if (p_typ == "function_call" || p_typ == "custom_tool_call") && call_id == tool_id {
                    input = Some(st_codex_tool_input(p));
                } else if (p_typ == "function_call_output" || p_typ == "custom_tool_call_output")
                    && call_id == tool_id
                {
                    if let Some((text, _errored)) = st_codex_tool_output(p) {
                        result =
                            Some(st_extract_lines(&text, 20).unwrap_or(serde_json::Value::Null));
                    }
                }
            }
        }
        if input.is_some() && result.is_some() {
            break;
        }
    }
    let body = serde_json::json!({
        "input": input.unwrap_or_else(|| serde_json::json!({})),
        "result": result.unwrap_or(serde_json::Value::Null),
    });
    serde_json::to_vec(&body).map_err(|e| e.to_string())
}

// Cheap variant for polling: just the file size + mtime. Lets Transcript
// detect changes without re-fetching the full (multi-MB) JSONL each
// tick. The frontend then bumps a cache-busting param to trigger a
// real fetch only when size has changed.
fn read_latest_session_meta<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> Result<Vec<u8>, String> {
    let Some(path) = latest_session_path(app, preferred)? else {
        return Ok(b"null".to_vec());
    };
    let md = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let body = format!(r#"{{"size":{},"mtime":{},"now":{}}}"#, md.len(), mtime, now);
    Ok(body.into_bytes())
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b;
        let unreserved =
            c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.' || c == b'~';
        if unreserved {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{:02X}", c));
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn mime_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("ico") => "image/x-icon",
        // Tauri's bundled-frontend protocol stamps unknown extensions as text/html,
        // which xmlui-standalone rejects. Serve them as text/plain instead.
        Some("xmlui") | Some("xs") => "text/plain; charset=utf-8",
        Some("md") => "text/markdown; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// Markers used to identify the Bram block inside a project's CLAUDE.md and
// AGENTS.md. The block contains the imported/embedded worklist guidance;
// future runs of run_enhance replace what's between the markers without
// disturbing surrounding content. Legacy xmlui-desktop markers are still
// recognized and migrated to the Bram marker pair on the next Setup run.
const ENHANCE_MARKER_START: &str = "<!-- bram:start -->";
const ENHANCE_MARKER_END: &str = "<!-- bram:end -->";
const ENHANCE_LEGACY_MARKER_START: &str = "<!-- xmlui-desktop:start -->";
const ENHANCE_LEGACY_MARKER_END: &str = "<!-- xmlui-desktop:end -->";
const ENHANCE_SIDECAR_REL: &str = ".claude/bram-conventions.md";
// Pre-bram rename. Setup migrates legacy path to ENHANCE_SIDECAR_REL on
// next run; status / shellrc / profile / codex guard all accept either
// filename so projects that haven't re-run Setup still register as
// Bram-managed during the transition.
const ENHANCE_SIDECAR_LEGACY_REL: &str = ".claude/xmlui-desktop-conventions.md";
const ENHANCE_CODEX_AGENTS_REL: &str = "AGENTS.md";
const ENHANCE_CODEX_BUNDLE_REL: &str = "shell/codex-startup-instructions.md";
const ENHANCE_HOOK_SCRIPT_REL: &str = ".claude/hooks/worklist-guard.py";
const ENHANCE_SETTINGS_REL: &str = ".claude/settings.json";
const ENHANCE_HOOK_BUNDLE_REL: &str = "__shell/worklist-guard.py";
// Codex's worklist guard runs as a PreToolUse hook in codex's user-global
// config. The bundle ships with Bram and is copied to
// ~/.bram/codex-worklist-guard.py the first time setup runs in any
// project; the hook config registration in ~/.codex/config.toml is identical
// across projects because the script self-detects whether the active cwd is
// Bram-managed (presence of resources/.worklist-authorization.json).
const ENHANCE_CODEX_HOOK_BUNDLE_REL: &str = "shell/worklist-guard-codex.py";
const ENHANCE_CODEX_HOOK_INSTALL_REL: &str = ".bram/codex-worklist-guard.py";
const ENHANCE_CODEX_TRUST_ACK_REL: &str = ".bram/codex-trust-ack";
const ENHANCE_CODEX_CONFIG_REL: &str = ".codex/config.toml";
// TOML-comment markers delimit the Bram block inside codex's
// config.toml so re-runs can replace it without disturbing surrounding entries.
const ENHANCE_CODEX_TOML_MARKER_START: &str = "# bram:start";
const ENHANCE_CODEX_TOML_MARKER_END: &str = "# bram:end";
const ENHANCE_CODEX_LEGACY_TOML_MARKER_START: &str = "# xmlui-desktop:start";
const ENHANCE_CODEX_LEGACY_TOML_MARKER_END: &str = "# xmlui-desktop:end";
// developer_instructions is a top-level scalar in config.toml. TOML requires
// top-level keys to come BEFORE any [section] table, so this block lives at
// the head of the file in its own marker. Verified via `codex debug
// prompt-input` to land in the developer-role context part between
// permissions_instructions and skills_instructions — a higher-priority slot
// than the user-role AGENTS.md path. That's why this surface carries the gate
// prose now instead of a per-turn UserPromptSubmit injection.
const ENHANCE_CODEX_INSTR_MARKER_START: &str = "# bram-instructions:start";
const ENHANCE_CODEX_INSTR_MARKER_END: &str = "# bram-instructions:end";
const ENHANCE_CODEX_LEGACY_INSTR_MARKER_START: &str = "# xmlui-desktop-instructions:start";
const ENHANCE_CODEX_LEGACY_INSTR_MARKER_END: &str = "# xmlui-desktop-instructions:end";
const ENHANCE_CODEX_TYPO_INSTR_MARKER_END: &str = "# brraminstructions:end";
const CLAUDE_CURL_ALLOW_PATTERNS: &[&str] = &[
    "Bash(curl -4 -sS --retry-connrefused --retry 3 --retry-delay 1 \"http://127.0.0.1*__worklist*)",
    "Bash(curl -4 -sS --retry-connrefused --retry 3 --retry-delay 1 -X POST \"http://127.0.0.1*__worklist*)",
    "Bash(curl -4 -sS --retry-connrefused --retry 3 --retry-delay 1 -X POST * \"http://127.0.0.1*__worklist*)",
    "Bash(curl -4 -sS --retry-connrefused --retry 3 --retry-delay 1 \"http://127.0.0.1*__iterate*)",
    "Bash(curl -4 -sS --retry-connrefused --retry 3 --retry-delay 1 -X POST \"http://127.0.0.1*__iterate*)",
    "Bash(curl -4 -sS --retry-connrefused --retry 3 --retry-delay 1 -X POST * \"http://127.0.0.1*__iterate*)",
    "Bash(curl -4 -sS --retry-connrefused --retry 3 --retry-delay 1 \"http://127.0.0.1*__issue*)",
    "Bash(curl -4 -sS --retry-connrefused --retry 3 --retry-delay 1 \"http://127.0.0.1*__enhance*)",
];
// Compact high-priority gate prose embedded in the Bram binary. Keep detailed
// lifecycle rules in app/__shell/conventions.md to avoid drift.
const ENHANCE_CODEX_GATE_PROSE: &str = "bram worklist gate. \
Use the worklist for material file/code changes unless the user explicitly \
opts out in a way the runtime guard allows. Mutations outside approved items \
are blocked by a PreToolUse hook installed under ~/.bram. On approved:, drop:, \
or iterate: turns, drive the Bram lifecycle through the filesystem channel: \
write resources/.worklist-intent.json ({nonce, route, body}) and read the \
host's reply from resources/.worklist-result.json, matching your nonce. Do \
not silently continue after a missing result or an ok:false reply. The exact \
routes, intent/result shapes, opt-out, and transition rules are canonical in \
app/__shell/conventions.md. Do not duplicate or guess those details from this \
abbreviated instruction.";
const WORKLIST_AUTH_REL: &str = "resources/.worklist-authorization.json";
// Codex filesystem lifecycle channel (#130). Codex writes the intent file;
// the host watcher drains it and writes the result file. Coordination
// dot-files, polled/written like the others above — not tracked changes.
const WORKLIST_INTENT_REL: &str = "resources/.worklist-intent.json";
const WORKLIST_RESULT_REL: &str = "resources/.worklist-result.json";
// Host-managed inflight sentinel (#84). Written when /__worklist/resolve
// serves an approved or drop record, OR when /__iterate/begin is
// called. Approved/drop sentinels clear at host silence-detected
// turn-end, with /__worklist/end available as an explicit endpoint.
// Iterate sentinels clear via /__iterate/end. The iframe derives its
// spinner state from this file and the [inflight-sentinel] trace makes
// the lifecycle verifiable.
const INFLIGHT_CLAIM_REL: &str = "resources/.inflight-claim.json";
// Right-pane pty-intent relay (#86). Append-only JSONL queue persisted
// to disk so right-pane clicks (toShell / toTurn / sendKeys) survive an
// iframe-reload-mid-click. Drained synchronously by queue_pty_intent;
// startup cleanup deletes any stale queue from a prior session.
const PTY_INTENT_REL: &str = "resources/.pty-intent.jsonl";
// On Unix, the bare path runs via the script's `#!/usr/bin/env python3`
// shebang (set executable by run_enhance under #[cfg(unix)]). On Windows
// there's no shebang resolution and no chmod, so we invoke through the
// `py` launcher — it ships with the python.org installer and resolves
// Python via the registry, independent of PATH.
#[cfg(windows)]
const ENHANCE_HOOK_COMMAND: &str = "py -3 \"$CLAUDE_PROJECT_DIR/.claude/hooks/worklist-guard.py\"";
#[cfg(not(windows))]
const ENHANCE_HOOK_COMMAND: &str = "$CLAUDE_PROJECT_DIR/.claude/hooks/worklist-guard.py";
// Presence of this file in the project root means the project IS the Bram
// source repo (it bundles the conventions). enhance_status treats it as a
// valid sidecar location; run_enhance skips the parts that would otherwise
// self-overwrite the source.
const ENHANCE_SOURCE_BUNDLE_REL: &str = "app/__shell/conventions.md";

fn settings_has_worklist_guard_hook(settings_path: &Path) -> bool {
    let content = match std::fs::read_to_string(settings_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    value
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hs| {
                        hs.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|s| s.contains("worklist-guard.py"))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

// Remove any PreToolUse hook entries whose `command` contains
// `proposal-guard.py` (the pre-rename script name, see bc3ee31). Hooks
// arrays emptied by the prune are dropped; entries with no remaining
// hooks are removed. Returns Ok(true) if anything was changed, Ok(false)
// if there was nothing to prune (or the file/JSON shape made it a no-op).
fn prune_proposal_guard_from_settings(settings_path: &Path) -> Result<bool, String> {
    let existing = match std::fs::read_to_string(settings_path) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    if existing.trim().is_empty() {
        return Ok(false);
    }
    let mut value: serde_json::Value = serde_json::from_str(&existing)
        .map_err(|e| format!("parse {}: {}", settings_path.display(), e))?;
    let Some(pre_arr) = value
        .get_mut("hooks")
        .and_then(|h| h.get_mut("PreToolUse"))
        .and_then(|p| p.as_array_mut())
    else {
        return Ok(false);
    };
    let mut changed = false;
    pre_arr.retain_mut(|entry| {
        let Some(hooks_arr) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
            return true;
        };
        let before = hooks_arr.len();
        hooks_arr.retain(|h| {
            !h.get("command")
                .and_then(|c| c.as_str())
                .map(|s| s.contains("proposal-guard.py"))
                .unwrap_or(false)
        });
        if hooks_arr.len() != before {
            changed = true;
        }
        !hooks_arr.is_empty()
    });
    if !changed {
        return Ok(false);
    }
    let serialized = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("serialize settings.json: {}", e))?;
    std::fs::write(settings_path, format!("{}\n", serialized))
        .map_err(|e| format!("write {}: {}", settings_path.display(), e))?;
    Ok(true)
}

fn merge_claude_curl_allowlist_into_settings(settings_path: &Path) -> Result<bool, String> {
    let existing = std::fs::read_to_string(settings_path).unwrap_or_default();
    let mut value: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&existing)
            .map_err(|e| format!("parse {}: {}", settings_path.display(), e))?
    };
    if !value.is_object() {
        return Err(format!(
            "{} root is not a JSON object",
            settings_path.display()
        ));
    }
    let root = value.as_object_mut().unwrap();
    let permissions = root
        .entry("permissions".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !permissions.is_object() {
        return Err(format!(
            "{}: permissions is not a JSON object",
            settings_path.display()
        ));
    }
    let permissions_obj = permissions.as_object_mut().unwrap();
    let allow = permissions_obj
        .entry("allow".to_string())
        .or_insert_with(|| serde_json::json!([]));
    if !allow.is_array() {
        return Err(format!(
            "{}: permissions.allow is not a JSON array",
            settings_path.display()
        ));
    }
    let allow_arr = allow.as_array_mut().unwrap();
    let before = allow_arr.len();
    allow_arr.retain(|entry| {
        let Some(s) = entry.as_str() else {
            return true;
        };
        !(s.starts_with("Bash(curl -sS")
            && (s.contains("__worklist") || s.contains("__iterate") || s.contains("__enhance")))
    });
    let mut changed = allow_arr.len() != before;
    for pattern in CLAUDE_CURL_ALLOW_PATTERNS {
        if !allow_arr
            .iter()
            .any(|entry| entry.as_str() == Some(*pattern))
        {
            allow_arr.push(serde_json::Value::String((*pattern).to_string()));
            changed = true;
        }
    }
    if !changed {
        return Ok(false);
    }
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    let serialized = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("serialize settings.json: {}", e))?;
    std::fs::write(settings_path, format!("{}\n", serialized))
        .map_err(|e| format!("write {}: {}", settings_path.display(), e))?;
    Ok(true)
}

// Ensure settings.json contains exactly one PreToolUse hook entry whose
// command matches ENHANCE_HOOK_COMMAND for this platform, preserving
// other keys. Existing worklist-guard.py entries with a different
// command string (e.g. the pre-cfg-windows bare path on a Windows
// project that was set up before the py-launcher migration) are
// removed and the correct entry is appended. Returns Ok(true) if any
// change was made, Ok(false) if the settings already had the exact
// entry and nothing needed migrating.
fn merge_worklist_guard_into_settings(settings_path: &Path) -> Result<bool, String> {
    let existing = std::fs::read_to_string(settings_path).unwrap_or_default();
    let mut value: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&existing)
            .map_err(|e| format!("parse {}: {}", settings_path.display(), e))?
    };
    if !value.is_object() {
        return Err(format!(
            "{} root is not a JSON object",
            settings_path.display()
        ));
    }
    let root = value.as_object_mut().unwrap();
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        return Err(format!(
            "{}: hooks is not a JSON object",
            settings_path.display()
        ));
    }
    let hooks_obj = hooks.as_object_mut().unwrap();
    let pre = hooks_obj
        .entry("PreToolUse".to_string())
        .or_insert_with(|| serde_json::json!([]));
    if !pre.is_array() {
        return Err(format!(
            "{}: hooks.PreToolUse is not a JSON array",
            settings_path.display()
        ));
    }
    let pre_arr = pre.as_array_mut().unwrap();

    // Drop worklist-guard.py entries whose command differs from the
    // current platform's ENHANCE_HOOK_COMMAND. Migrates an existing
    // bare-path Windows install to `py -3 ...` (and would also handle
    // the reverse if a project moved between platforms).
    let mut migrated = false;
    pre_arr.retain_mut(|entry| {
        let Some(hooks_arr) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
            return true;
        };
        let before = hooks_arr.len();
        hooks_arr.retain(|h| {
            let Some(cmd) = h.get("command").and_then(|c| c.as_str()) else {
                return true;
            };
            !(cmd.contains("worklist-guard.py") && cmd != ENHANCE_HOOK_COMMAND)
        });
        if hooks_arr.len() != before {
            migrated = true;
        }
        !hooks_arr.is_empty()
    });

    let exact_present = pre_arr.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hs| {
                hs.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|s| s == ENHANCE_HOOK_COMMAND)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });
    if exact_present && !migrated {
        return Ok(false);
    }
    if !exact_present {
        pre_arr.push(serde_json::json!({
            "matcher": "Write|Edit",
            "hooks": [{
                "type": "command",
                "command": ENHANCE_HOOK_COMMAND,
            }]
        }));
    }
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    let serialized = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("serialize settings.json: {}", e))?;
    std::fs::write(settings_path, format!("{}\n", serialized))
        .map_err(|e| format!("write {}: {}", settings_path.display(), e))?;
    Ok(true)
}

// Provider-aware Context tab. Claude shows the project-local import chain
// plus Claude-managed memory/hooks/settings. Codex shows the durable local
// Codex-side sources that shape behavior on this machine: config, project
// files, memories, and rules.

struct ContextFile {
    category: &'static str,
    path: PathBuf,
    display: String,
    kind: Option<&'static str>,
}

fn context_provider<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> SessionProvider {
    preferred
        .or_else(|| hinted_session_provider(app))
        .unwrap_or(SessionProvider::Claude)
}

fn collect_claude_context_files<R: tauri::Runtime>(app: &AppHandle<R>) -> Vec<ContextFile> {
    let mut out: Vec<ContextFile> = Vec::new();
    let Some(proj_root) = project_root(Some(app)) else {
        return out;
    };

    let claude_md = proj_root.join("CLAUDE.md");
    if claude_md.exists() {
        out.push(ContextFile {
            category: "project",
            path: claude_md.clone(),
            display: "CLAUDE.md".to_string(),
            kind: Some("claude-md"),
        });
        if let Ok(content) = std::fs::read_to_string(&claude_md) {
            for line in content.lines() {
                let trimmed = line.trim();
                let import_path = match trimmed.strip_prefix('@') {
                    Some(p) if !p.is_empty() && !p.starts_with(char::is_whitespace) => p,
                    _ => continue,
                };
                let abs = proj_root.join(import_path);
                if abs.exists() {
                    out.push(ContextFile {
                        category: "project",
                        path: abs,
                        display: import_path.to_string(),
                        kind: Some("import"),
                    });
                }
            }
        }
    }

    if let Some(home) = home_dir() {
        let memory_dir = home
            .join(".claude")
            .join("projects")
            .join(encode_path_for_filename(&proj_root))
            .join("memory");
        if memory_dir.is_dir() {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(&memory_dir)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect();
            paths.sort();
            for path in paths {
                let display = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                out.push(ContextFile {
                    category: "memory",
                    path,
                    display,
                    kind: None,
                });
            }
        }
    }

    let hooks_dir = proj_root.join(".claude").join("hooks");
    if hooks_dir.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&hooks_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        paths.sort();
        for path in paths {
            let display = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            out.push(ContextFile {
                category: "hooks",
                path,
                display,
                kind: None,
            });
        }
    }

    let claude_dir = proj_root.join(".claude");
    for name in ["settings.json", "settings.local.json"] {
        let path = claude_dir.join(name);
        if path.exists() {
            out.push(ContextFile {
                category: "settings",
                path,
                display: name.to_string(),
                kind: None,
            });
        }
    }

    out
}

fn collect_codex_context_files<R: tauri::Runtime>(app: &AppHandle<R>) -> Vec<ContextFile> {
    let mut out: Vec<ContextFile> = Vec::new();
    let Some(proj_root) = project_root(Some(app)) else {
        return out;
    };
    let Some(home) = home_dir() else {
        return out;
    };

    let codex_dir = home.join(".codex");
    let config_toml = codex_dir.join("config.toml");
    let agents_md = proj_root.join("AGENTS.md");
    if agents_md.exists() {
        out.push(ContextFile {
            category: "project",
            path: agents_md,
            display: "AGENTS.md".to_string(),
            kind: Some("codex-agents"),
        });
    }
    if config_toml.exists() {
        out.push(ContextFile {
            category: "project",
            path: config_toml,
            display: "config.toml".to_string(),
            kind: Some("codex-config"),
        });
    }

    let project_dot_codex = proj_root.join(".codex");
    if project_dot_codex.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&project_dot_codex)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        paths.sort();
        for path in paths {
            let display = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            out.push(ContextFile {
                category: "project",
                path,
                display,
                kind: Some("project-codex"),
            });
        }
    }

    let memories_dir = codex_dir.join("memories");
    if memories_dir.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&memories_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        paths.sort();
        for path in paths {
            let display = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            out.push(ContextFile {
                category: "memory",
                path,
                display,
                kind: None,
            });
        }
    }

    let rules_dir = codex_dir.join("rules");
    if rules_dir.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&rules_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        paths.sort();
        for path in paths {
            let display = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            out.push(ContextFile {
                category: "rules",
                path,
                display,
                kind: None,
            });
        }
    }

    out
}

fn collect_context_files<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> Vec<ContextFile> {
    match context_provider(app, preferred) {
        SessionProvider::Claude => collect_claude_context_files(app),
        SessionProvider::Codex => collect_codex_context_files(app),
    }
}

// Group the flat ContextFile list into category buckets for the Context tab's
// left-pane list.
fn context_list<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
) -> serde_json::Value {
    use serde_json::json;
    let provider = context_provider(app, preferred);
    let mut project: Vec<serde_json::Value> = Vec::new();
    let mut memory: Vec<serde_json::Value> = Vec::new();
    let mut hooks: Vec<serde_json::Value> = Vec::new();
    let mut settings: Vec<serde_json::Value> = Vec::new();
    let mut rules: Vec<serde_json::Value> = Vec::new();
    for f in collect_context_files(app, Some(provider)) {
        let mut item = json!({
            "path": f.path.to_string_lossy(),
            "display": f.display,
        });
        if let Some(k) = f.kind {
            item["kind"] = json!(k);
        }
        match f.category {
            "project" => project.push(item),
            "memory" => memory.push(item),
            "hooks" => hooks.push(item),
            "settings" => settings.push(item),
            "rules" => rules.push(item),
            _ => {}
        }
    }

    let (provider_name, summary, sections) = match provider {
        SessionProvider::Claude => (
            "claude",
            "Claude Context shows the repo-local `CLAUDE.md` import chain plus Claude-managed memory, hooks, and settings for this project.",
            vec![
                json!({ "key": "project", "label": "Project", "items": project }),
                json!({ "key": "memory", "label": "Memory", "items": memory }),
                json!({ "key": "hooks", "label": "Hooks", "items": hooks }),
                json!({ "key": "settings", "label": "Settings", "items": settings }),
            ],
        ),
        SessionProvider::Codex => (
            "codex",
            "Codex Context shows the repo-local `AGENTS.md` instructions when present plus durable Codex-side sources on this machine, such as `~/.codex/config.toml`, project-local `.codex/` files, memories, and rules.",
            vec![
                json!({ "key": "project", "label": "Project", "items": project }),
                json!({ "key": "memory", "label": "Memories", "items": memory }),
                json!({ "key": "rules", "label": "Rules", "items": rules }),
            ],
        ),
    };
    json!({ "provider": provider_name, "summary": summary, "sections": sections })
}

// Case-insensitive substring search across the same file set as
// context_list. Returns groups of { path, display, category, hits: [{ line,
// snippet }] }. Capped at 50 total hits to keep payloads bounded.
fn context_search<R: tauri::Runtime>(
    app: &AppHandle<R>,
    preferred: Option<SessionProvider>,
    q: &str,
) -> serde_json::Value {
    use serde_json::json;
    let provider = context_provider(app, preferred);
    let needle = q.trim().to_lowercase();
    if needle.is_empty() {
        return json!({
            "provider": match provider {
                SessionProvider::Claude => "claude",
                SessionProvider::Codex => "codex",
            },
            "results": []
        });
    }
    const MAX_HITS: usize = 50;
    let mut total_hits = 0usize;
    let mut results: Vec<serde_json::Value> = Vec::new();
    for file in collect_context_files(app, Some(provider)) {
        if total_hits >= MAX_HITS {
            break;
        }
        let content = match std::fs::read_to_string(&file.path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut hits: Vec<serde_json::Value> = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if total_hits >= MAX_HITS {
                break;
            }
            if line.to_lowercase().contains(&needle) {
                let snippet: String = line.trim().chars().take(200).collect();
                hits.push(json!({
                    "line": i + 1,
                    "snippet": snippet,
                }));
                total_hits += 1;
            }
        }
        if !hits.is_empty() {
            results.push(json!({
                "category": file.category,
                "path": file.path.to_string_lossy(),
                "display": file.display,
                "hits": hits,
            }));
        }
    }
    json!({
        "provider": match provider {
            SessionProvider::Claude => "claude",
            SessionProvider::Codex => "codex",
        },
        "results": results,
        "truncated": total_hits >= MAX_HITS
    })
}

// Compare the on-disk copy of a hook script against the bundled copy
// embedded in this binary. Returns false if the on-disk file is missing,
// unreadable, or differs by even one byte from the bundle. Used by
// enhance_status to flip claude_installed / codex_installed false when a
// previously-set-up project still has a stale hook from an older release —
// without this, the Setup button stays hidden after an upgrade and
// enhance_run never re-fires to overwrite the stale file.
fn hook_matches_bundle<R: tauri::Runtime>(
    app: &AppHandle<R>,
    on_disk: &Path,
    bundle_rel: &str,
) -> bool {
    let Ok(disk_bytes) = std::fs::read(on_disk) else {
        return false;
    };
    let Some((bundle_bytes, _)) = serve_app_file(Some(app), bundle_rel) else {
        return false;
    };
    disk_bytes == bundle_bytes
}

fn extract_marker_block<'a>(disk: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let start_idx = disk.find(start)?;
    let tail = &disk[start_idx..];
    let end_off = tail.find(end)?;
    let end_idx = start_idx + end_off + end.len();
    Some(&disk[start_idx..end_idx])
}

fn codex_agents_block_current<R: tauri::Runtime>(
    app: &AppHandle<R>,
    agents_path: &Path,
    is_source_repo: bool,
) -> bool {
    // Source repo carries AGENTS.md as the canonical content, not as a
    // Bram-installed marker block. Setup doesn't rewrite it, so it can't
    // go stale relative to the bundle.
    if is_source_repo {
        return true;
    }
    let Some((bundle_bytes, _)) = serve_app_file(Some(app), ENHANCE_CODEX_BUNDLE_REL) else {
        return false;
    };
    let Ok(seed) = String::from_utf8(bundle_bytes) else {
        return false;
    };
    let expected = format!(
        "{}\n{}\n{}",
        ENHANCE_MARKER_START,
        seed.trim_end(),
        ENHANCE_MARKER_END
    );
    let Ok(disk) = std::fs::read_to_string(agents_path) else {
        return false;
    };
    // Legacy marker presence-only counts as stale: next Refresh will
    // swap it for the new marker via replace_or_append_managed_block.
    extract_marker_block(&disk, ENHANCE_MARKER_START, ENHANCE_MARKER_END)
        .map(|slice| slice == expected)
        .unwrap_or(false)
}

fn codex_instr_block_current(config_path: &Path) -> bool {
    let expected = format!(
        "{start}\ndeveloper_instructions = {body}\n{end}",
        start = ENHANCE_CODEX_INSTR_MARKER_START,
        end = ENHANCE_CODEX_INSTR_MARKER_END,
        body = toml_basic_string(ENHANCE_CODEX_GATE_PROSE),
    );
    let Ok(disk) = std::fs::read_to_string(config_path) else {
        return false;
    };
    extract_marker_block(
        &disk,
        ENHANCE_CODEX_INSTR_MARKER_START,
        ENHANCE_CODEX_INSTR_MARKER_END,
    )
    .map(|slice| slice == expected)
    .unwrap_or(false)
}

fn enhance_status<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<Vec<u8>, String> {
    use serde_json::json;
    let proj = project_root(Some(app)).ok_or("no project root")?;
    let claude_md = proj.join("CLAUDE.md");
    let codex_agents = proj.join(ENHANCE_CODEX_AGENTS_REL);
    let sidecar = proj.join(ENHANCE_SIDECAR_REL);
    let hook_script = proj.join(ENHANCE_HOOK_SCRIPT_REL);
    let settings = proj.join(ENHANCE_SETTINGS_REL);
    let worklist_auth = proj.join(WORKLIST_AUTH_REL);
    let codex_hook_script = home_dir().map(|h| h.join(ENHANCE_CODEX_HOOK_INSTALL_REL));
    let active_provider = hinted_session_provider(app);
    let is_source_repo = proj.join(ENHANCE_SOURCE_BUNDLE_REL).exists();
    let claude_md_has_marker = std::fs::read_to_string(&claude_md)
        .map(|s| {
            s.contains(ENHANCE_MARKER_START)
                || s.contains(ENHANCE_LEGACY_MARKER_START)
                || (is_source_repo && s.contains("@app/__shell/conventions.md"))
        })
        .unwrap_or(false);
    // Source repo treats the bundle itself as the canonical sidecar.
    // Legacy .claude/xmlui-desktop-conventions.md also counts as installed
    // until Setup migrates it to the new path.
    let sidecar_exists =
        sidecar.exists() || proj.join(ENHANCE_SIDECAR_LEGACY_REL).exists() || is_source_repo;
    let hook_script_exists = hook_script.exists();
    let hook_script_current =
        hook_script_exists && hook_matches_bundle(app, &hook_script, ENHANCE_HOOK_BUNDLE_REL);
    let hook_registered = settings_has_worklist_guard_hook(&settings);
    let codex_agents_has_marker = std::fs::read_to_string(&codex_agents)
        .map(|s| {
            s.contains(ENHANCE_MARKER_START)
                || s.contains(ENHANCE_LEGACY_MARKER_START)
                || (is_source_repo
                    && s.contains("This repo is driven through Bram")
                    && s.contains("resources/worklist.json"))
        })
        .unwrap_or(false);
    let codex_hook_current = codex_hook_script
        .as_ref()
        .map(|p| hook_matches_bundle(app, p, ENHANCE_CODEX_HOOK_BUNDLE_REL))
        .unwrap_or(false);
    let codex_agents_current = codex_agents_block_current(app, &codex_agents, is_source_repo);
    let codex_config_path = home_dir().map(|h| h.join(ENHANCE_CODEX_CONFIG_REL));
    let codex_instr_current = codex_config_path
        .as_ref()
        .map(|p| codex_instr_block_current(p))
        .unwrap_or(false);
    let codex_trust_ack = home_dir()
        .and_then(|h| {
            let stored = std::fs::read_to_string(h.join(ENHANCE_CODEX_TRUST_ACK_REL)).ok()?;
            let current = hook_fingerprint(&h.join(ENHANCE_CODEX_HOOK_INSTALL_REL))?;
            Some(stored.trim() == current)
        })
        .unwrap_or(false);
    let core_installed = worklist_auth.exists();
    let claude_installed =
        claude_md_has_marker && sidecar_exists && hook_script_current && hook_registered;
    let codex_installed = core_installed
        && codex_agents_has_marker
        && codex_agents_current
        && codex_hook_current
        && codex_instr_current;
    let codex_install_stale_only = core_installed
        && codex_agents_has_marker
        && (!codex_hook_current || !codex_agents_current || !codex_instr_current);
    let claude_needs_setup = !core_installed || !claude_installed;
    let codex_needs_setup = !core_installed || !codex_installed;
    let provider_needs_setup = match active_provider {
        Some(SessionProvider::Claude) => claude_needs_setup,
        Some(SessionProvider::Codex) => codex_needs_setup,
        None => false,
    };
    let active_provider_json = match active_provider {
        Some(SessionProvider::Claude) => json!("claude"),
        Some(SessionProvider::Codex) => json!("codex"),
        None => serde_json::Value::Null,
    };
    let body = serde_json::json!({
        "enhanced": core_installed && claude_installed && codex_installed,
        "activeProvider": active_provider_json,
        "coreInstalled": core_installed,
        "claudeInstalled": claude_installed,
        "codexInstalled": codex_installed,
        "claudeNeedsSetup": claude_needs_setup,
        "codexNeedsSetup": codex_needs_setup,
        "providerNeedsSetup": provider_needs_setup,
        "codexInstallStaleOnly": codex_install_stale_only,
        "providerSetupKind": if matches!(active_provider, Some(SessionProvider::Codex)) && codex_install_stale_only {
            "codex-hook-refresh"
        } else {
            "repo-setup"
        },
        "claudeMd": claude_md_has_marker,
        "codexAgents": codex_agents_has_marker,
        "sidecar": sidecar_exists,
        "hookScript": hook_script_exists,
        "hookScriptCurrent": hook_script_current,
        "codexHookCurrent": codex_hook_current,
        "codexAgentsCurrent": codex_agents_current,
        "codexInstrCurrent": codex_instr_current,
        "codexTrustAck": codex_trust_ack,
        "hookRegistered": hook_registered,
        "fallbackMode": "watcher-revert",
        "claudeMdPath": claude_md.display().to_string(),
        "codexAgentsPath": codex_agents.display().to_string(),
        "sidecarPath": sidecar.display().to_string(),
        "hookScriptPath": hook_script.display().to_string(),
        "settingsPath": settings.display().to_string(),
        "worklistAuthPath": worklist_auth.display().to_string(),
    });
    serde_json::to_vec(&body).map_err(|e| e.to_string())
}

/// Write `new_content` to `path` only when safe to do so without trampling
/// user-modified disk content. Returns `Ok(true)` if a write occurred,
/// `Ok(false)` if skipped (either no-op or divergence preserved).
///
/// Semantics:
///   - disk == new_content → no-op, returns Ok(false), neither list updated.
///   - disk missing       → write, push to `wrote`, returns Ok(true).
///   - disk differs && force → write, push to `wrote`, returns Ok(true).
///   - disk differs && !force → preserve, push to `skipped`, returns Ok(false).
fn write_template_if_safe(
    path: &Path,
    new_content: &[u8],
    force: bool,
    wrote: &mut Vec<String>,
    skipped: &mut Vec<String>,
) -> Result<bool, String> {
    match std::fs::read(path) {
        Ok(existing) if existing == new_content => Ok(false),
        Ok(_) => {
            if force {
                std::fs::write(path, new_content)
                    .map_err(|e| format!("write {}: {}", path.display(), e))?;
                wrote.push(path.display().to_string());
                Ok(true)
            } else {
                skipped.push(path.display().to_string());
                Ok(false)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(path, new_content)
                .map_err(|e2| format!("write {}: {}", path.display(), e2))?;
            wrote.push(path.display().to_string());
            Ok(true)
        }
        Err(e) => Err(format!("read {}: {}", path.display(), e)),
    }
}

fn run_enhance<R: tauri::Runtime>(app: &AppHandle<R>, force: bool) -> Result<Vec<u8>, String> {
    let proj = project_root(Some(app)).ok_or("no project root")?;
    // When running on the source repo, skip writes that would
    // self-overwrite (recreating the deleted local sidecar, reverting
    // the @-import path in CLAUDE.md). Idempotent installs (hook
    // script, settings.json merge) still run.
    let is_source_repo = proj.join(ENHANCE_SOURCE_BUNDLE_REL).exists();

    let mut wrote: Vec<String> = Vec::new();
    // Paths whose on-disk content diverges from the bundle and which Setup
    // chose to preserve (default behavior). Empty when force=true or when
    // disk == bundle at every managed path. Surfaced in the response so the
    // agent-tools-drawer can warn the user.
    let mut skipped: Vec<String> = Vec::new();

    // Provider-neutral worklist authorization cache. Bram records the
    // latest structured `approved:` / `drop:` payload here so the desktop-side
    // watcher can enforce the two-stage worklist policy even when the active
    // provider has no native pre-tool hook support.
    let worklist_auth_path = proj.join(WORKLIST_AUTH_REL);
    if let Some(parent) = worklist_auth_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    if !worklist_auth_path.exists() {
        let core_stub = WorklistAuthorizationRecord {
            kind: "none".to_string(),
            ids: Vec::new(),
            items: Vec::new(),
            mismatched_ids: Vec::new(),
            issued_at_ms: 0,
            source: "setup".to_string(),
            consumed_at_ms: None,
        };
        let serialized_core = serde_json::to_string_pretty(&core_stub)
            .map_err(|e| format!("serialize worklist authorization stub: {}", e))?;
        std::fs::write(&worklist_auth_path, format!("{}\n", serialized_core))
            .map_err(|e| format!("write {}: {}", worklist_auth_path.display(), e))?;
        wrote.push(worklist_auth_path.display().to_string());
    }

    // Empty worklist.json — created here so setup is the single on-ramp for
    // worklist-driven coordination. init_worklist_file and /__worklist/init
    // remain available but the UI no longer surfaces a manual init button.
    if let Some(worklist_path) = worklist_file(app) {
        if !worklist_path.exists() {
            std::fs::write(&worklist_path, empty_worklist_json())
                .map_err(|e| format!("write {}: {}", worklist_path.display(), e))?;
            if let Ok(mut guard) = last_worklist_cell().lock() {
                *guard = Some(empty_worklist_json().to_string());
            }
            wrote.push(worklist_path.display().to_string());
        }
    }

    // Conventions sidecar — skipped on the source repo.
    let sidecar_path = proj.join(ENHANCE_SIDECAR_REL);
    if !is_source_repo {
        let (conventions_bytes, _mime) = serve_app_file(Some(app), "__shell/conventions.md")
            .ok_or_else(|| "conventions template not found".to_string())?;
        let conventions = String::from_utf8(conventions_bytes)
            .map_err(|e| format!("conventions template not utf-8: {}", e))?;
        if let Some(parent) = sidecar_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {}", parent.display(), e))?;
        }
        // Always write in non-source repos — the sidecar is a whole-file
        // Setup-managed bundle with no user-edit story, and conventions.md
        // changes ship every release. Issue #173.
        write_template_if_safe(
            &sidecar_path,
            conventions.as_bytes(),
            true,
            &mut wrote,
            &mut skipped,
        )?;
        // Migration: remove the legacy sidecar so the project doesn't end
        // up with two convention files. NotFound is fine (legacy install
        // wasn't there, or already migrated).
        let legacy_sidecar = proj.join(ENHANCE_SIDECAR_LEGACY_REL);
        let _ = std::fs::remove_file(&legacy_sidecar);
    }

    // AGENTS.md marker block — skipped on the source repo (its AGENTS.md is
    // the canonical hand-maintained content, not a Setup-managed target).
    let codex_agents_path = proj.join(ENHANCE_CODEX_AGENTS_REL);
    if !is_source_repo {
        let (codex_seed_bytes, _mime) = serve_app_file(Some(app), ENHANCE_CODEX_BUNDLE_REL)
            .ok_or_else(|| "codex startup instructions bundle not found".to_string())?;
        let codex_seed = String::from_utf8(codex_seed_bytes)
            .map_err(|e| format!("codex startup instructions not utf-8: {}", e))?;
        let codex_block = format!(
            "{}\n{}\n{}",
            ENHANCE_MARKER_START,
            codex_seed.trim_end(),
            ENHANCE_MARKER_END
        );
        let existing_agents = std::fs::read_to_string(&codex_agents_path).unwrap_or_default();
        let new_agents = replace_or_append_managed_block(&existing_agents, &codex_block);
        // Force when legacy markers are on disk — divergence is the
        // Setup-managed marker rename, not a user edit. Issue #173.
        let migrate = existing_agents.contains(ENHANCE_LEGACY_MARKER_START);
        write_template_if_safe(
            &codex_agents_path,
            new_agents.as_bytes(),
            force || migrate,
            &mut wrote,
            &mut skipped,
        )?;
    }

    // Proposal-guard hook script. In the source repo, content-comparison gates
    // the write so a committed in-flight diagnostic-logging edit to the hook
    // survives Setup (the case #99 was designed for). In non-source repos the
    // hook has no documented user-extension point, so bundle bumps must always
    // land — passing force=true here avoids stranding projects on a stale hook
    // every release that touches the bundle. Issue #173. chmod still runs only
    // after an actual write — preserves the user's mode if we skipped.
    let (hook_bytes, _mime) = serve_app_file(Some(app), ENHANCE_HOOK_BUNDLE_REL)
        .ok_or_else(|| "worklist-guard.py bundle not found".to_string())?;
    let hook_path = proj.join(ENHANCE_HOOK_SCRIPT_REL);
    if let Some(parent) = hook_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    let hook_written = write_template_if_safe(
        &hook_path,
        &hook_bytes,
        force || !is_source_repo,
        &mut wrote,
        &mut skipped,
    )?;
    #[cfg(unix)]
    if hook_written {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)
            .map_err(|e| format!("stat hook: {}", e))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms).map_err(|e| format!("chmod hook: {}", e))?;
    }

    // Pre-rename leftover script (bc3ee31). Idempotent: NotFound is fine.
    let old_hook_path = proj.join(".claude/hooks/proposal-guard.py");
    let _ = std::fs::remove_file(&old_hook_path);

    // Register hook in settings.json (idempotent merge). Prune any
    // pre-rename proposal-guard.py PreToolUse entries first so upgraded
    // projects don't end up running both hooks on every Write/Edit.
    let settings_path = proj.join(ENHANCE_SETTINGS_REL);
    prune_proposal_guard_from_settings(&settings_path)?;
    merge_claude_curl_allowlist_into_settings(&settings_path)?;
    merge_worklist_guard_into_settings(&settings_path)?;
    wrote.push(settings_path.display().to_string());

    // CLAUDE.md marker block — skipped on the source repo.
    let claude_md_path = proj.join("CLAUDE.md");
    if !is_source_repo {
        let existing = std::fs::read_to_string(&claude_md_path).unwrap_or_default();
        let block = format!(
            "{}\n@{}\n{}",
            ENHANCE_MARKER_START, ENHANCE_SIDECAR_REL, ENHANCE_MARKER_END
        );
        let new_content = replace_or_append_managed_block(&existing, &block);
        // Force when legacy markers are on disk — divergence is the
        // Setup-managed marker rename and the @-import path swap onto the
        // new sidecar file, not a user edit. Issue #173.
        let migrate = existing.contains(ENHANCE_LEGACY_MARKER_START);
        write_template_if_safe(
            &claude_md_path,
            new_content.as_bytes(),
            force || migrate,
            &mut wrote,
            &mut skipped,
        )?;
    }

    // Codex user-global hook install. Runs unconditionally (incl. source repo)
    // because the install is keyed to $HOME, not the project.
    let codex_hook_install = install_codex_worklist_guard(app)?;
    for path in &codex_hook_install.wrote {
        wrote.push(path.clone());
    }
    for path in &codex_hook_install.skipped {
        skipped.push(path.clone());
    }
    // Developer-instructions install — top-level config.toml scalar carrying
    // the gate prose. Replaced the per-turn UserPromptSubmit injection after
    // verifying developer-role context is salient enough on its own.
    install_codex_developer_instructions()?;

    let body = serde_json::json!({
        "enhanced": true,
        "isSourceRepo": is_source_repo,
        "wrote": wrote,
        "divergedSkipped": skipped,
        "codexHookInstalled": codex_hook_install.installed,
        "codexHookScriptPath": codex_hook_install.script_path,
        "codexConfigPath": codex_hook_install.config_path,
        "codexHookNeedsTrust": codex_hook_install.installed,
    });
    serde_json::to_vec(&body).map_err(|e| e.to_string())
}

fn hook_fingerprint(path: &Path) -> Option<String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path).ok()?;
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    Some(format!("{:016x}", h.finish()))
}

fn write_codex_trust_ack() -> Result<(), String> {
    let home = home_dir().ok_or("no HOME or USERPROFILE")?;
    let hook = home.join(ENHANCE_CODEX_HOOK_INSTALL_REL);
    let fp = hook_fingerprint(&hook)
        .ok_or_else(|| format!("read {}: hook not installed", hook.display()))?;
    let marker = home.join(ENHANCE_CODEX_TRUST_ACK_REL);
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
    }
    std::fs::write(&marker, fp.as_bytes())
        .map_err(|e| format!("write {}: {}", marker.display(), e))?;
    Ok(())
}

struct CodexHookInstall {
    installed: bool,
    script_path: String,
    config_path: String,
    wrote: Vec<String>,
    skipped: Vec<String>,
}

fn install_codex_worklist_guard<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<CodexHookInstall, String> {
    let home = home_dir().ok_or("no HOME or USERPROFILE")?;
    let script_path = home.join(ENHANCE_CODEX_HOOK_INSTALL_REL);
    let config_path = home.join(ENHANCE_CODEX_CONFIG_REL);
    let mut wrote: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    let (script_bytes, _mime) = serve_app_file(Some(app), ENHANCE_CODEX_HOOK_BUNDLE_REL)
        .ok_or_else(|| "worklist-guard-codex.py bundle not found".to_string())?;
    if let Some(parent) = script_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    // Whole-file Setup-managed bundle; force-write so bundle bumps land on
    // existing installs. No source-repo carve-out — this hook installs to
    // $HOME/.bram/ on every machine. Issue #174.
    let codex_hook_written =
        write_template_if_safe(&script_path, &script_bytes, true, &mut wrote, &mut skipped)?;
    #[cfg(unix)]
    if codex_hook_written {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)
            .map_err(|e| format!("stat codex hook: {}", e))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms)
            .map_err(|e| format!("chmod codex hook: {}", e))?;
    }

    // Build the TOML block. On Windows we invoke through `py -3` for the
    // same reason as the Claude hook; on Unix we run the script directly via
    // its shebang. The matcher regex covers codex's canonical apply_patch +
    // Bash and the Claude-style Write/Edit aliases codex accepts.
    let script_str = script_path.display().to_string();
    #[cfg(windows)]
    let command_line = format!("py -3 \"{}\"", script_str.replace('"', "\\\""));
    #[cfg(not(windows))]
    let command_line = script_str.clone();
    // Matcher covers codex's canonical apply_patch + Bash, the Claude-style
    // Write/Edit aliases codex accepts, and any MCP tool (mcp__<server>__<tool>).
    // The MCP surface matters: a user with [mcp_servers.filesystem] configured
    // can route file edits through mcp__filesystem__write_text_file / edit_file
    // and bypass apply_patch entirely. The guard script branches by tool_name
    // and only blocks MCP calls whose names signal mutation (write/edit/create/
    // delete/move/...).
    //
    // The pre-emptive nudge (was UserPromptSubmit injection earlier) is now
    // carried by `developer_instructions` at top-level config — verified to
    // be rendered in the developer-role context part, higher priority than
    // AGENTS.md (which is user-role). install_codex_developer_instructions
    // writes that field; this function only installs the runtime backstop.
    let toml_block = format!(
        "{start}\n\
         [[hooks.PreToolUse]]\n\
         matcher = \"^(apply_patch|Bash|Write|Edit|mcp__.*)$\"\n\
         \n\
         [[hooks.PreToolUse.hooks]]\n\
         type = \"command\"\n\
         command = {command_quoted}\n\
         timeout = 10\n\
         statusMessage = \"Bram worklist guard\"\n\
         {end}",
        start = ENHANCE_CODEX_TOML_MARKER_START,
        end = ENHANCE_CODEX_TOML_MARKER_END,
        command_quoted = toml_basic_string(&command_line),
    );

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let cleaned = strip_marker_block(
        &existing,
        ENHANCE_CODEX_LEGACY_TOML_MARKER_START,
        ENHANCE_CODEX_LEGACY_TOML_MARKER_END,
    );
    let new_content = if let Some(start_idx) = cleaned.find(ENHANCE_CODEX_TOML_MARKER_START) {
        let tail = &cleaned[start_idx..];
        let end_offset = tail
            .find(ENHANCE_CODEX_TOML_MARKER_END)
            .map(|i| start_idx + i + ENHANCE_CODEX_TOML_MARKER_END.len())
            .unwrap_or(cleaned.len());
        let mut s = cleaned.clone();
        s.replace_range(start_idx..end_offset, &toml_block);
        s
    } else if cleaned.trim().is_empty() {
        format!("{}\n", toml_block)
    } else {
        format!("{}\n\n{}\n", cleaned.trim_end(), toml_block)
    };
    // Force-write the merged result so bundle-format changes (matcher regex,
    // timeout, command shape) land on existing installs. The strip-and-insert
    // merge above already preserves user-authored TOML outside the marker
    // block. Issue #174.
    write_template_if_safe(
        &config_path,
        new_content.as_bytes(),
        true,
        &mut wrote,
        &mut skipped,
    )?;

    Ok(CodexHookInstall {
        installed: true,
        script_path: script_path.display().to_string(),
        config_path: config_path.display().to_string(),
        wrote,
        skipped,
    })
}

fn install_codex_developer_instructions() -> Result<(), String> {
    let home = home_dir().ok_or("no HOME or USERPROFILE")?;
    let config_path = home.join(ENHANCE_CODEX_CONFIG_REL);
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();

    // Strip managed legacy blocks so setup replaces them with the Bram block
    // instead of creating duplicate top-level `developer_instructions` keys.
    let cleaned = strip_marker_block(
        &existing,
        "# xmlui-desktop-test-instr:start",
        "# xmlui-desktop-test-instr:end",
    );
    let cleaned = strip_marker_block(
        &cleaned,
        ENHANCE_CODEX_LEGACY_INSTR_MARKER_START,
        ENHANCE_CODEX_LEGACY_INSTR_MARKER_END,
    );
    let cleaned = if cleaned.contains(ENHANCE_CODEX_TYPO_INSTR_MARKER_END) {
        strip_marker_block(
            &cleaned,
            ENHANCE_CODEX_INSTR_MARKER_START,
            ENHANCE_CODEX_TYPO_INSTR_MARKER_END,
        )
    } else {
        cleaned
    };

    let block = format!(
        "{start}\ndeveloper_instructions = {body}\n{end}",
        start = ENHANCE_CODEX_INSTR_MARKER_START,
        end = ENHANCE_CODEX_INSTR_MARKER_END,
        body = toml_basic_string(ENHANCE_CODEX_GATE_PROSE),
    );

    let new_content = if let Some(start_idx) = cleaned.find(ENHANCE_CODEX_INSTR_MARKER_START) {
        let tail = &cleaned[start_idx..];
        let end_offset = tail
            .find(ENHANCE_CODEX_INSTR_MARKER_END)
            .map(|i| start_idx + i + ENHANCE_CODEX_INSTR_MARKER_END.len())
            .unwrap_or(cleaned.len());
        let mut s = cleaned.clone();
        s.replace_range(start_idx..end_offset, &block);
        s
    } else if cleaned.trim().is_empty() {
        format!("{}\n", block)
    } else {
        // Prepend at top of file. developer_instructions is a top-level scalar
        // and TOML requires those before any [section] table.
        format!("{}\n\n{}", block, cleaned.trim_start_matches('\n'))
    };
    std::fs::write(&config_path, &new_content)
        .map_err(|e| format!("write codex config.toml: {}", e))?;
    Ok(())
}

fn strip_marker_block(content: &str, start: &str, end: &str) -> String {
    let mut result = content.to_string();
    while let Some(start_idx) = result.find(start) {
        let tail = &result[start_idx..];
        let end_offset = match tail.find(end) {
            Some(i) => start_idx + i + end.len(),
            None => result.len(),
        };
        // Also consume the trailing newline if present, so we don't leave a blank line.
        let mut cut_to = end_offset;
        if result.as_bytes().get(cut_to) == Some(&b'\n') {
            cut_to += 1;
        }
        result.replace_range(start_idx..cut_to, "");
    }
    result
}

fn replace_or_append_managed_block(existing: &str, block: &str) -> String {
    for (start, end) in [
        (ENHANCE_MARKER_START, ENHANCE_MARKER_END),
        (ENHANCE_LEGACY_MARKER_START, ENHANCE_LEGACY_MARKER_END),
    ] {
        if let Some(start_idx) = existing.find(start) {
            let tail = &existing[start_idx..];
            let end_offset = tail
                .find(end)
                .map(|i| start_idx + i + end.len())
                .unwrap_or(existing.len());
            let mut s = existing.to_string();
            s.replace_range(start_idx..end_offset, block);
            return s;
        }
    }
    if existing.is_empty() {
        format!("{}\n", block)
    } else {
        format!("{}\n\n{}\n", existing.trim_end(), block)
    }
}

// Quote a string as a TOML basic string literal — wraps in double quotes and
// escapes backslashes / double quotes / control chars per TOML spec.
fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ============================================================================
// Worklist history (issue #18: save and browse completed worklist items)
//
// The filesystem watcher detects writes to resources/worklist.json and
// appends a timestamped JSON snapshot of the *prior* contents to
// resources/worklist-history/, plus a sibling .md changelog summarizing
// what changed. The cache lives in process memory so we can diff old
// vs new without re-reading from disk.
// ============================================================================

static LAST_WORKLIST: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static LAST_WORKLIST_EFFECTIVE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
const HISTORY_DIFF_MAX_LINES: usize = 80;
const HISTORY_DIFF_MAX_BYTES: usize = 4 * 1024;
const WORKLIST_HISTORY_DEFAULT_LIMIT: usize = 120;

fn last_worklist_cell() -> &'static Mutex<Option<String>> {
    LAST_WORKLIST.get_or_init(|| Mutex::new(None))
}

fn last_worklist_effective_cell() -> &'static Mutex<Option<String>> {
    LAST_WORKLIST_EFFECTIVE.get_or_init(|| Mutex::new(None))
}

fn worklist_file<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_resource_path(app, "worklist.json")
}

fn worklist_drafts_dir<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_resource_path(app, "worklist-drafts")
}

fn worklist_auth_file<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_relative_path(app, WORKLIST_AUTH_REL)
}

fn feedback_drafts_dir<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_resource_path(app, "feedback-drafts")
}

fn feedback_history_dir<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_resource_path(app, "feedback-history")
}

fn project_relative_path<R: tauri::Runtime, P: AsRef<Path>>(
    app: &AppHandle<R>,
    rel: P,
) -> Option<PathBuf> {
    project_root(Some(app)).map(|p| p.join(rel))
}

fn resource_relative_path<P: AsRef<Path>>(root: &Path, rel: P) -> PathBuf {
    root.join("resources").join(rel)
}

fn project_resource_path<R: tauri::Runtime, P: AsRef<Path>>(
    app: &AppHandle<R>,
    rel: P,
) -> Option<PathBuf> {
    project_root(Some(app)).map(|p| resource_relative_path(&p, rel))
}

fn draft_markdown_path(dir: &Path, id: &str) -> Option<PathBuf> {
    if id.is_empty() || id.contains('/') || id.contains('\\') {
        return None;
    }
    Some(dir.join(format!("{}.md", id)))
}

fn feedback_draft_path(drafts_dir: &Path, feedback_id: &str) -> Option<PathBuf> {
    draft_markdown_path(drafts_dir, feedback_id)
}

fn feedback_draft_belongs_to_item(file_name: &str, item_id: &str) -> bool {
    file_name == format!("{}.md", item_id) || file_name.ends_with(&format!("-{}.md", item_id))
}

// List recent feedback-history entries for the Feedback tab. Filenames
// are <unix_ms>-<itemId>.md (per unique_feedback_history_path); a
// uniqueness suffix `-<n>` may appear before `.md` on the rare collision
// path. Parse ts as a leading numeric prefix and itemId as the remainder
// between the first `-` and trailing `.md`. Reverse-chronological by ts.
fn recent_feedback_history<R: tauri::Runtime>(
    app: &AppHandle<R>,
    limit: usize,
) -> Vec<serde_json::Value> {
    let Some(dir) = feedback_history_dir(app) else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut rows: Vec<(u64, String, String)> = Vec::new();
    for entry in read.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let stem = match name.strip_suffix(".md") {
            Some(s) => s,
            None => continue,
        };
        let (ts_str, item_id) = match stem.split_once('-') {
            Some(p) => p,
            None => continue,
        };
        let ts: u64 = match ts_str.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        rows.push((ts, item_id.to_string(), name));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter()
        .take(limit)
        .map(|(ts, item_id, file_name)| {
            serde_json::json!({
                "ts": ts,
                "itemId": item_id,
                "fileName": file_name,
            })
        })
        .collect()
}

fn unique_feedback_history_path(history_dir: &Path, file_name: &str) -> PathBuf {
    let initial = history_dir.join(file_name);
    if !initial.exists() {
        return initial;
    }
    let stem = file_name.strip_suffix(".md").unwrap_or(file_name);
    for n in 1..1000 {
        let candidate = history_dir.join(format!("{}-{}.md", stem, n));
        if !candidate.exists() {
            return candidate;
        }
    }
    history_dir.join(format!("{}-{}.md", stem, unix_now_ms()))
}

fn promote_feedback_drafts_for_items<R: tauri::Runtime>(
    app: &AppHandle<R>,
    item_ids: &[String],
    reason: &str,
) {
    if item_ids.is_empty() {
        return;
    }
    let Some(drafts_dir) = feedback_drafts_dir(app) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&drafts_dir) else {
        return;
    };
    let Some(history_dir) = feedback_history_dir(app) else {
        return;
    };
    let mut matched: Vec<(PathBuf, String, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        if let Some(item_id) = item_ids
            .iter()
            .find(|id| feedback_draft_belongs_to_item(&file_name, id))
        {
            matched.push((path, file_name, item_id.clone()));
        }
    }
    if matched.is_empty() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&history_dir) {
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "feedback-draft",
                &format!("op=promote-dir-failed reason={} error={}", reason, e),
            );
        }
        return;
    }
    for (path, file_name, item_id) in matched {
        let dest = unique_feedback_history_path(&history_dir, &file_name);
        match std::fs::rename(&path, &dest) {
            Ok(()) => {
                if bram_trace_enabled() {
                    append_bram_trace_line(
                        app,
                        "feedback-draft",
                        &format!(
                            "op=promote reason={} item_id={} feedback_id={}",
                            reason,
                            item_id,
                            file_name.strip_suffix(".md").unwrap_or(&file_name)
                        ),
                    );
                }
            }
            Err(e) => {
                if bram_trace_enabled() {
                    append_bram_trace_line(
                        app,
                        "feedback-draft",
                        &format!(
                            "op=promote-failed reason={} item_id={} feedback_id={} error={}",
                            reason,
                            item_id,
                            file_name.strip_suffix(".md").unwrap_or(&file_name),
                            e
                        ),
                    );
                }
            }
        }
    }
}

fn worklist_history_dir<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_resource_path(app, "worklist-history")
}

fn inflight_claim_file<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_relative_path(app, INFLIGHT_CLAIM_REL)
}

#[derive(Clone, Default)]
struct TurnCompletionMonitor {
    source: String,
    provider: String,
    reason: String,
    detail: String,
    seen_at_ms: i64,
    claimed_after: bool,
}

fn turn_completion_monitor_cell() -> &'static Mutex<TurnCompletionMonitor> {
    static CELL: OnceLock<Mutex<TurnCompletionMonitor>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(TurnCompletionMonitor::default()))
}

fn record_turn_completion_monitor(
    source: &str,
    provider: &str,
    reason: &str,
    detail: String,
    seen_at_ms: i64,
    claimed_after: bool,
) {
    if let Ok(mut monitor) = turn_completion_monitor_cell().lock() {
        *monitor = TurnCompletionMonitor {
            source: source.to_string(),
            provider: provider.to_string(),
            reason: reason.to_string(),
            detail,
            seen_at_ms,
            claimed_after,
        };
    }
}

fn current_turn_completion_monitor() -> TurnCompletionMonitor {
    turn_completion_monitor_cell()
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default()
}

fn worklist_intent_file<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_relative_path(app, WORKLIST_INTENT_REL)
}

fn worklist_result_file<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_relative_path(app, WORKLIST_RESULT_REL)
}

// Write the inflight sentinel (#84). Atomic via .tmp + rename so the
// file is either absent or contains valid JSON. Caller has verified
// `ids` is non-empty. `kind` is one of "approved", "drop", "iterate".
fn write_inflight_claim_sentinel<R: tauri::Runtime>(
    app: &AppHandle<R>,
    ids: &[String],
    kind: &str,
) {
    let Some(path) = inflight_claim_file(app) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = serde_json::json!({
        "ids": ids,
        "claimedAt": unix_now_ms(),
        "kind": kind,
    });
    let body = match serde_json::to_string_pretty(&payload) {
        Ok(s) => s,
        Err(_) => return,
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, format!("{}\n", body)).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, &path);
    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "inflight-sentinel",
            &format!(
                "op=write kind={} ids={}",
                kind,
                serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_string())
            ),
        );
    }
    emit_replayable_signal(app, "inflight-claim-changed");
}

fn inflight_claim_ids_and_claimed_at<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Option<(Vec<String>, i64)> {
    let path = inflight_claim_file(app)?;
    let content = std::fs::read_to_string(&path).ok()?;
    let claim: serde_json::Value = serde_json::from_str(&content).ok()?;
    let ids: Vec<String> = claim
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let claimed_at = claim.get("claimedAt").and_then(|v| v.as_i64()).unwrap_or(0);
    Some((ids, claimed_at))
}

// Clear the inflight sentinel (#84). Conditions: a sentinel exists,
// AND every id currently claimed is in `mutated_ids`. Partial coverage
// leaves the sentinel alone — partial completion is a diagnostic
// signal worth surfacing (stuck spinner = stuck claim once item 3
// lands).
// Pure: is a sentinel claim (its claimed ids) fully covered by the mutated
// ids? Empty/absent claims count as not covered. Split out so the clear/emit
// decision is unit-testable without an AppHandle (refs #133).
fn inflight_claim_fully_covered(claimed_ids: &[String], mutated_ids: &[String]) -> bool {
    !claimed_ids.is_empty()
        && claimed_ids
            .iter()
            .all(|cid| mutated_ids.iter().any(|mid| mid == cid))
}

// Returns true iff a covering sentinel was found, removed, and the
// inflight-claim-changed signal emitted; false on every early return (no file,
// parse failure, empty/uncovered claim). A caller that reaches a completion
// point without a prior sentinel (refs #133) uses the false return to emit its
// own reconcile signal so the iframe still clears optimistic `submitting`.
fn clear_inflight_claim_sentinel<R: tauri::Runtime>(
    app: &AppHandle<R>,
    mutated_ids: &[String],
) -> bool {
    let Some(path) = inflight_claim_file(app) else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    let claim: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let claimed_ids: Vec<String> = claim
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if !inflight_claim_fully_covered(&claimed_ids, mutated_ids) {
        return false;
    }
    let _ = std::fs::remove_file(&path);
    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "inflight-sentinel",
            &format!(
                "op=clear ids={}",
                serde_json::to_string(&claimed_ids).unwrap_or_else(|_| "[]".to_string())
            ),
        );
    }
    emit_replayable_signal(app, "inflight-claim-changed");

    // Flush a deferred tools-pane-reload if one was queued during the
    // cycle (refs #93). Atomic swap-to-false; the previous value tells
    // us whether to fire.
    if PENDING_TOOLS_RELOAD.swap(false, std::sync::atomic::Ordering::SeqCst) {
        if bram_trace_enabled() {
            append_bram_trace_line(app, "tools-pane-reload", "op=flushed-on-clear");
        }
        // Replayable: same late-attach reasoning as right-pane-reload
        // above. A reload-on-attach refreshes the agent-tools drawer
        // iframe with current state, which is idempotent. Refs #170.
        emit_replayable_signal(app, "tools-pane-reload");
    }
    true
}

// True iff resources/.inflight-claim.json exists, parses, and lists at
// least one claimed id. Used by the watcher to decide whether to emit
// tools-pane-reload now or defer it until the cycle clears (refs #93).
fn inflight_sentinel_is_active<R: tauri::Runtime>(app: &AppHandle<R>) -> bool {
    let Some(path) = inflight_claim_file(app) else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    let claim: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    claim
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|arr| !arr.is_empty())
        .unwrap_or(false)
}

// Emit tools-pane-reload, OR defer it if a cycle is currently active
// (refs #93). The host owns the cycle-active signal via the inflight
// sentinel; suppressing reloads during cycles prevents iframe remount
// from blowing away the user's mid-cycle context and causing the 7+s
// of drift / click swallows we measured pre-fix.
fn emit_or_defer_tools_pane_reload<R: tauri::Runtime>(app: &AppHandle<R>) {
    if inflight_sentinel_is_active(app) {
        PENDING_TOOLS_RELOAD.store(true, std::sync::atomic::Ordering::SeqCst);
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "tools-pane-reload",
                "op=deferred reason=sentinel-active",
            );
        }
        return;
    }
    trace_emit_signal(app, "tools-pane-reload");
    let _ = app.emit("tools-pane-reload", ());
}

// Read the current sentinel's claimed ids and call the regular clear
// path with them — used by the agent-turn-end hook to fire a clear
// without the caller having to know who's claimed. No-op if the
// sentinel is absent or has no ids. Refs #91 follow-up.
fn clear_active_sentinel<R: tauri::Runtime>(app: &AppHandle<R>) {
    if let Some((ids, _claimed_at)) = inflight_claim_ids_and_claimed_at(app) {
        clear_inflight_claim_sentinel(app, &ids);
    }
}

fn clear_active_sentinel_with_reason<R: tauri::Runtime>(app: &AppHandle<R>, reason: &str) {
    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "inflight-sentinel",
            &format!("op=clear-request reason={}", reason),
        );
    }
    let active_claim = inflight_claim_ids_and_claimed_at(app);
    let claimed_after = active_claim
        .as_ref()
        .map(|(ids, claimed_at)| !ids.is_empty() && unix_now_ms() >= *claimed_at)
        .unwrap_or(false);
    record_turn_completion_monitor(
        "cancel",
        "pty",
        reason,
        "PTY cancellation path requested active sentinel clear".to_string(),
        unix_now_ms(),
        claimed_after,
    );
    if let Some((ids, _claimed_at)) = active_claim {
        clear_inflight_claim_sentinel(app, &ids);
    } else {
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "inflight-sentinel",
                &format!("op=clear-miss-emitted reason={}", reason),
            );
        }
        emit_replayable_signal(app, "inflight-claim-changed");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JsonlCompletionProvider {
    Claude,
    Codex,
}

impl JsonlCompletionProvider {
    fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct JsonlCompletionDecision {
    detected: bool,
    reason: &'static str,
}

fn jsonl_completion_provider_for_path(path: &std::path::Path) -> Option<JsonlCompletionProvider> {
    let path_str = path.to_string_lossy();
    if path_str.contains("/.claude/") {
        Some(JsonlCompletionProvider::Claude)
    } else if path_str.contains("/.codex/") {
        Some(JsonlCompletionProvider::Codex)
    } else {
        None
    }
}

fn jsonl_file_basename(path: &std::path::Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn format_inflight_claim_for_trace<R: tauri::Runtime>(app: &AppHandle<R>) -> String {
    inflight_claim_ids_and_claimed_at(app)
        .map(|(ids, claimed_at)| {
            if ids.is_empty() {
                "empty".to_string()
            } else {
                format!(
                    "ids={} claimedAt={}",
                    serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_string()),
                    claimed_at
                )
            }
        })
        .unwrap_or_else(|| "none".to_string())
}

fn trace_jsonl_detector_handoff<R: tauri::Runtime>(
    app: &AppHandle<R>,
    source: &str,
    path: &std::path::Path,
    mtime_ms: i64,
) {
    if !bram_trace_enabled() {
        return;
    }
    let provider = jsonl_completion_provider_for_path(path)
        .map(|p| p.label())
        .unwrap_or("unknown");
    append_bram_trace_line(
        app,
        "jsonl-turn-end",
        &format!(
            "op=poll-handoff source={} provider={} path={} mtime={} activeClaim={}",
            source,
            provider,
            jsonl_file_basename(path),
            mtime_ms,
            format_inflight_claim_for_trace(app)
        ),
    );
}

fn jsonl_tail_shape(content: &str, max_records: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            out.push("unparseable".to_string());
            if out.len() >= max_records {
                break;
            }
            continue;
        };
        let typ = entry
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if typ == "event_msg" {
            let payload_type = entry
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            out.push(format!("event_msg:{}", payload_type));
        } else {
            out.push(typ.to_string());
        }
        if out.len() >= max_records {
            break;
        }
    }
    out.reverse();
    if out.is_empty() {
        "empty".to_string()
    } else {
        out.join(",")
    }
}

fn claude_jsonl_completion_decision(content: &str) -> JsonlCompletionDecision {
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if entry.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let stop_reason = entry
            .get("message")
            .and_then(|m| m.get("stop_reason"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return JsonlCompletionDecision {
            detected: stop_reason == "end_turn",
            reason: if stop_reason == "end_turn" {
                "end_turn"
            } else {
                "non-final-assistant"
            },
        };
    }
    JsonlCompletionDecision {
        detected: false,
        reason: "no-assistant-record",
    }
}

fn codex_jsonl_completion_decision(content: &str) -> JsonlCompletionDecision {
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(entry_type) = entry.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        if entry_type == "event_msg" {
            let payload_type = entry
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if payload_type == "task_complete" {
                return JsonlCompletionDecision {
                    detected: true,
                    reason: "task_complete",
                };
            }
            if payload_type == "task_started" || payload_type == "user_message" {
                return JsonlCompletionDecision {
                    detected: false,
                    reason: "next-task-started",
                };
            }
            continue;
        }
        if entry_type == "response_item" || entry_type == "turn_context" {
            continue;
        }
        return JsonlCompletionDecision {
            detected: false,
            reason: "non-final-record",
        };
    }
    JsonlCompletionDecision {
        detected: false,
        reason: "no-completion-record",
    }
}

fn jsonl_completion_decision(
    provider: JsonlCompletionProvider,
    content: &str,
) -> JsonlCompletionDecision {
    match provider {
        JsonlCompletionProvider::Claude => claude_jsonl_completion_decision(content),
        JsonlCompletionProvider::Codex => codex_jsonl_completion_decision(content),
    }
}

// JSONL-driven turn-end detection (#91 follow-up, broadened by #153).
// Durable session output is the primary independent completion signal:
// Claude writes assistant `stop_reason:"end_turn"`; Codex writes
// `event_msg`/`task_complete`. PTY silence remains a fallback.
fn check_jsonl_for_turn_end<R: tauri::Runtime>(app: &AppHandle<R>, path: &std::path::Path) {
    let provider = match jsonl_completion_provider_for_path(path) {
        Some(p) => p,
        None => {
            if bram_trace_enabled() {
                append_bram_trace_line(
                    app,
                    "jsonl-turn-end",
                    &format!(
                        "op=skip provider=unknown reason=unrecognized-path path={}",
                        jsonl_file_basename(path)
                    ),
                );
            }
            return;
        }
    };
    let provider_label = provider.label();
    let basename = jsonl_file_basename(path);
    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "jsonl-turn-end",
            &format!("op=enter provider={} path={}", provider_label, basename),
        );
    }

    let Ok(metadata) = std::fs::metadata(path) else {
        record_turn_completion_monitor(
            "jsonl",
            provider_label,
            "metadata-read-failed",
            format!("Could not stat {}", basename),
            unix_now_ms(),
            false,
        );
        return;
    };
    let file_mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let Ok(content) = std::fs::read_to_string(path) else {
        record_turn_completion_monitor(
            "jsonl",
            provider_label,
            "read-failed",
            format!("Could not read {}", basename),
            file_mtime_ms,
            false,
        );
        return;
    };
    let decision = jsonl_completion_decision(provider, &content);
    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "jsonl-turn-end",
            &format!(
                "op=scan provider={} path={} tailTypes={}",
                provider_label,
                basename,
                jsonl_tail_shape(&content, 6)
            ),
        );
    }

    // Provider-agnostic finished-cue emit on detection. Anchors the
    // row's working→finished transition on the authoritative JSONL
    // boundary, independent of inflight-claim coordination below
    // (which only manages the spinner sentinel). For Claude, scrape
    // the current PTY tail for the natural-end banner — if present,
    // it supplies the finished cue's verb + duration; if absent
    // (resize artifact, banner not yet painted, partial chunk), the
    // row shows generic "Finished". Refs #179.
    let active_matches = matches!(
        (hinted_session_provider(app), provider),
        (
            Some(SessionProvider::Claude),
            JsonlCompletionProvider::Claude
        ) | (Some(SessionProvider::Codex), JsonlCompletionProvider::Codex)
    );
    if decision.detected && active_matches {
        match provider {
            JsonlCompletionProvider::Claude => {
                let (verb, elapsed) = pty_tail_cell()
                    .lock()
                    .ok()
                    .map(|tail| strip_ansi(&tail))
                    .as_ref()
                    .and_then(|s| parse_claude_natural_end_banner(s))
                    .map(|b| (Some(b.verb), Some(b.duration_text)))
                    .unwrap_or((None, None));
                if bram_trace_enabled() {
                    append_bram_trace_line(
                        app,
                        "finished-cue",
                        &format!(
                            "source=jsonl provider=claude verb={} elapsed={}",
                            verb.as_deref().unwrap_or("(none)"),
                            elapsed.as_deref().unwrap_or("(none)"),
                        ),
                    );
                }
                kill_current_claude_turn_with_finished(app, verb, elapsed);
            }
            JsonlCompletionProvider::Codex => {
                if bram_trace_enabled() {
                    append_bram_trace_line(
                        app,
                        "finished-cue",
                        "source=jsonl provider=codex verb=(none) elapsed=(none)",
                    );
                }
                agent_status_emit_finished(app, provider_label, None, None);
            }
        }
    }

    let Some((claimed_ids, claimed_at)) = inflight_claim_ids_and_claimed_at(app) else {
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "jsonl-turn-end",
                &format!(
                    "op=skip provider={} reason=no-active-sentinel decision={} path={} mtime={}",
                    provider_label, decision.reason, basename, file_mtime_ms
                ),
            );
        }
        record_turn_completion_monitor(
            "jsonl",
            provider_label,
            "no-active-sentinel",
            format!("{} decision from {}", decision.reason, basename),
            file_mtime_ms,
            false,
        );
        return;
    };
    if claimed_ids.is_empty() {
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "jsonl-turn-end",
                &format!(
                    "op=skip provider={} reason=empty-sentinel decision={} path={} mtime={}",
                    provider_label, decision.reason, basename, file_mtime_ms
                ),
            );
        }
        record_turn_completion_monitor(
            "jsonl",
            provider_label,
            "empty-sentinel",
            format!("{} decision from {}", decision.reason, basename),
            file_mtime_ms,
            false,
        );
        return;
    }

    let claimed_after = file_mtime_ms >= claimed_at;
    if file_mtime_ms < claimed_at {
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "jsonl-turn-end",
                &format!(
                    "op=skip provider={} reason=stale-prior-turn decision={} path={} mtime={} claimed={}",
                    provider_label,
                    decision.reason,
                    basename,
                    file_mtime_ms,
                    serde_json::to_string(&claimed_ids).unwrap_or_else(|_| "[]".to_string())
                ),
            );
        }
        record_turn_completion_monitor(
            "jsonl",
            provider_label,
            "stale-prior-turn",
            format!(
                "{} from {} predates active claim",
                decision.reason, basename
            ),
            file_mtime_ms,
            false,
        );
        return;
    }

    if !decision.detected {
        if bram_trace_enabled() {
            append_bram_trace_line(
                app,
                "jsonl-turn-end",
                &format!(
                    "op=skip provider={} reason={} path={} mtime={} claimed={}",
                    provider_label,
                    decision.reason,
                    basename,
                    file_mtime_ms,
                    serde_json::to_string(&claimed_ids).unwrap_or_else(|_| "[]".to_string())
                ),
            );
        }
        record_turn_completion_monitor(
            "jsonl",
            provider_label,
            decision.reason,
            format!("No provider completion marker in {}", basename),
            file_mtime_ms,
            claimed_after,
        );
        return;
    }

    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "jsonl-turn-end",
            &format!(
                "op=detect provider={} reason={} path={} mtime={} claimed={}",
                provider_label,
                decision.reason,
                basename,
                file_mtime_ms,
                serde_json::to_string(&claimed_ids).unwrap_or_else(|_| "[]".to_string())
            ),
        );
    }
    record_turn_completion_monitor(
        "jsonl",
        provider_label,
        decision.reason,
        format!("Provider completion marker in {}", basename),
        file_mtime_ms,
        claimed_after,
    );

    // Note: agent_status_emit_finished for both providers fires from
    // the boundary-detected block earlier (above the inflight-claim
    // gate), so the row transitions on every real JSONL boundary
    // regardless of inflight coordination. Refs #179.
    clear_inflight_claim_sentinel(app, &claimed_ids);
}

// Startup cleanup. Removes any stale inflight sentinel from a prior
// session that didn't complete (Bram killed mid-cycle, agent crashed
// before mutate, etc.).
fn cleanup_stale_inflight_claim<R: tauri::Runtime>(app: &AppHandle<R>) {
    let Some(path) = inflight_claim_file(app) else {
        return;
    };
    if !path.exists() {
        return;
    }
    let _ = std::fs::remove_file(&path);
    if bram_trace_enabled() {
        append_bram_trace_line(app, "inflight-sentinel", "op=stale-startup-clear");
    }
    emit_replayable_signal(app, "inflight-claim-changed");
}

// Delete any leftover Codex lifecycle intent/result files from a prior
// session (#130), so a stale result can't be misread as a reply to a fresh
// intent. Mirrors cleanup_stale_inflight_claim.
fn cleanup_stale_worklist_intent<R: tauri::Runtime>(app: &AppHandle<R>) {
    for path in [worklist_intent_file(app), worklist_result_file(app)]
        .into_iter()
        .flatten()
    {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
            if bram_trace_enabled() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                append_bram_trace_line(
                    app,
                    "worklist-intent",
                    &format!("op=stale-startup-clear file={}", name),
                );
            }
        }
    }
}

fn pty_intent_file<R: tauri::Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    project_relative_path(app, PTY_INTENT_REL)
}

// Serializes append + drain in queue_pty_intent so concurrent calls
// don't race the read-then-truncate phase and lose intents.
static PTY_INTENT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn pty_intent_lock() -> &'static Mutex<()> {
    PTY_INTENT_LOCK.get_or_init(|| Mutex::new(()))
}

// Monotonic counter for `[pty-intent]` trace ids. Doesn't need to be
// globally unique — only readable within one session's trace log.
static PTY_INTENT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// Startup cleanup. Removes any stale pty-intent queue from a prior
// session so its intents don't replay into a fresh PTY (#86).
fn cleanup_stale_pty_intents<R: tauri::Runtime>(app: &AppHandle<R>) {
    let Some(path) = pty_intent_file(app) else {
        return;
    };
    if !path.exists() {
        return;
    }
    let _ = std::fs::remove_file(&path);
    if bram_trace_enabled() {
        append_bram_trace_line(app, "pty-intent", "op=stale-startup-clear");
    }
}

fn empty_worklist_json() -> &'static str {
    "{\n  \"description\": \"\",\n  \"items\": []\n}\n"
}

// Per-item content hash exposed via /__worklist. The UI reads it and
// propagates it verbatim into the `approved:` / `drop:` payload, so the
// PTY watcher can recompute the same fingerprint from the on-disk file
// and detect mid-flight drift without ever shipping full item content
// back into the conversation context.
//
// Canonicalization: serde_json is built WITHOUT the preserve_order
// feature in this crate, so its Map is BTreeMap-backed and
// `to_string` emits keys in sorted order at every depth. That gives
// us a deterministic byte sequence on both sides of the channel
// without needing a separate canonicalizer.
fn canonical_item_hash(item: &serde_json::Value) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let canonical = serde_json::to_string(item).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn parse_worklist_draft(raw: &str) -> Option<(String, String)> {
    enum Section {
        Before,
        After,
    }

    let mut section: Option<Section> = None;
    let mut before: Vec<&str> = Vec::new();
    let mut after: Vec<&str> = Vec::new();
    let mut saw_before = false;
    let mut saw_after = false;

    for line in raw.lines() {
        let marker = line.trim_end_matches('\r');
        if marker == "# Before" {
            saw_before = true;
            section = Some(Section::Before);
            continue;
        }
        if marker == "# After" {
            saw_after = true;
            section = Some(Section::After);
            continue;
        }
        match section {
            Some(Section::Before) => before.push(line),
            Some(Section::After) => after.push(line),
            None => {}
        }
    }

    if !saw_before || !saw_after {
        return None;
    }
    Some((
        before.join("\n").trim().to_string(),
        after.join("\n").trim().to_string(),
    ))
}

fn worklist_draft_path(drafts_dir: &Path, item_id: &str) -> Option<PathBuf> {
    draft_markdown_path(drafts_dir, item_id)
}

fn resolve_worklist_item_draft(
    drafts_dir: Option<&Path>,
    item: &serde_json::Value,
) -> serde_json::Value {
    // Draft-only: prose always comes from resources/worklist-drafts/<id>.md.
    // Inline `before`/`after` keys in worklist.json are rejected by both
    // guards, so we overwrite them here unconditionally. The merge that
    // used to short-circuit when both inline fields were non-empty has
    // been removed.
    let mut resolved = item.clone();
    let Some(obj) = resolved.as_object_mut() else {
        return resolved;
    };
    let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let draft = drafts_dir
        .and_then(|dir| worklist_draft_path(dir, item_id))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| parse_worklist_draft(&raw));

    if let Some((before, after)) = draft {
        obj.insert("before".to_string(), serde_json::Value::String(before));
        obj.insert("after".to_string(), serde_json::Value::String(after));
        obj.remove("_draftMissing");
    } else {
        obj.insert(
            "before".to_string(),
            serde_json::Value::String(String::new()),
        );
        obj.insert(
            "after".to_string(),
            serde_json::Value::String(String::new()),
        );
        obj.insert("_draftMissing".to_string(), serde_json::Value::Bool(true));
    }
    resolved
}

fn resolve_worklist_doc_drafts(
    drafts_dir: Option<&Path>,
    doc: &serde_json::Value,
) -> serde_json::Value {
    let mut resolved = doc.clone();
    if let Some(items) = resolved.get_mut("items").and_then(|v| v.as_array_mut()) {
        for item in items {
            *item = resolve_worklist_item_draft(drafts_dir, item);
        }
    }
    resolved
}

fn resolve_worklist_record_items<R: tauri::Runtime>(
    app: &AppHandle<R>,
    record: &mut serde_json::Value,
) {
    let drafts_dir = worklist_drafts_dir(app);
    if let Some(items) = record.get_mut("items").and_then(|v| v.as_array_mut()) {
        for item in items {
            *item = resolve_worklist_item_draft(drafts_dir.as_deref(), item);
        }
    }
}

fn base_worklist_doc_from_parsed(parsed_doc: Option<serde_json::Value>) -> serde_json::Value {
    use serde_json::json;

    match parsed_doc {
        Some(serde_json::Value::Object(obj)) => serde_json::Value::Object(obj),
        Some(serde_json::Value::Array(_)) => json!({
            "description": "",
            "items": [],
            "schemaError": "root-array",
            "schemaErrorMessage": "resources/worklist.json must be an object with { \"description\": string, \"items\": [] }, not a bare JSON array",
        }),
        Some(_) => json!({
            "description": "",
            "items": [],
            "schemaError": "root-non-object",
            "schemaErrorMessage": "resources/worklist.json must be a JSON object with { \"description\": string, \"items\": [] } at the root",
        }),
        None => json!({ "description": "", "items": [] }),
    }
}

fn worklist_doc<R: tauri::Runtime>(app: &AppHandle<R>) -> serde_json::Value {
    let path = worklist_file(app);
    let exists = path.as_ref().map_or(false, |p| p.is_file());
    let resources_exists = path
        .as_ref()
        .and_then(|p| p.parent())
        .map_or(false, |p| p.is_dir());
    let path_str = path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let parsed_doc: Option<serde_json::Value> = path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let mut doc = base_worklist_doc_from_parsed(parsed_doc);
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("exists".to_string(), serde_json::Value::Bool(exists));
        obj.insert(
            "resourcesExists".to_string(),
            serde_json::Value::Bool(resources_exists),
        );
        obj.insert("path".to_string(), serde_json::Value::String(path_str));
        if !obj.contains_key("description") {
            obj.insert(
                "description".to_string(),
                serde_json::Value::String(String::new()),
            );
        }
        if !obj.contains_key("items") {
            obj.insert("items".to_string(), serde_json::Value::Array(Vec::new()));
        }
        // Resolve draft-file prose before hashing so metadata-only worklist
        // items retain the same hash semantics as inline before/after items.
        if let Some(items) = obj.get_mut("items").and_then(|v| v.as_array_mut()) {
            let drafts_dir = worklist_drafts_dir(app);
            for item in items {
                *item = resolve_worklist_item_draft(drafts_dir.as_deref(), item);
                let hash = canonical_item_hash(item);
                if let Some(item_obj) = item.as_object_mut() {
                    item_obj.insert("hash".to_string(), serde_json::Value::String(hash));
                }
            }
        }
        doc
    } else {
        serde_json::json!({
            "description": "",
            "items": [],
            "exists": exists,
            "resourcesExists": resources_exists,
            "path": path_str,
        })
    }
}

fn coordination_ago(ms: i64, now: i64) -> String {
    if ms <= 0 {
        return "unknown".to_string();
    }
    let diff = (now - ms).max(0);
    if diff < 1000 {
        return "now".to_string();
    }
    let sec = (diff + 500) / 1000;
    if sec < 60 {
        return format!("{}s ago", sec);
    }
    let min = (sec + 30) / 60;
    if min < 60 {
        return format!("{}m ago", min);
    }
    let hr = (min + 30) / 60;
    if hr < 48 {
        return format!("{}h ago", hr);
    }
    format!("{}d ago", (hr + 12) / 24)
}

fn coordination_duration(ms: i64) -> String {
    let sec = (ms.max(0) + 500) / 1000;
    if sec < 60 {
        return format!("{}s", sec);
    }
    let min = sec / 60;
    let rem_sec = sec % 60;
    if min < 60 {
        if rem_sec == 0 {
            return format!("{}m", min);
        }
        return format!("{}m {}s", min, rem_sec);
    }
    let hr = min / 60;
    let rem_min = min % 60;
    if rem_min == 0 {
        format!("{}h", hr)
    } else {
        format!("{}h {}m", hr, rem_min)
    }
}

fn coordination_trace_line_iso(line: &str) -> String {
    line.strip_prefix('[')
        .and_then(|rest| rest.split_once(']').map(|(ts, _)| ts.to_string()))
        .unwrap_or_default()
}

fn trace_field_i64(line: &str, name: &str) -> Option<i64> {
    let token = format!("{}=", name);
    let start = line.find(&token)? + token.len();
    let rest = &line[start..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().ok()
}

fn trace_json_field_i64(line: &str, name: &str) -> Option<i64> {
    let token = format!("\"{}\":", name);
    let start = line.find(&token)? + token.len();
    let rest = &line[start..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().ok()
}

fn coordination_trace_summary(trace_text: &str) -> serde_json::Value {
    let lines: Vec<&str> = trace_text.lines().rev().take(5000).collect();
    let mut latest_tail_fresh = 0;
    let mut latest_tail_diff = 0;
    let mut latest_tail_bytes = 0;
    let mut fanout_events = 0;
    let mut fanout_resets = 0;
    let mut fanout_subscribers: Option<i64> = None;
    let mut cap_trims = 0;
    let mut inflight_writes = 0;
    let mut inflight_clears = 0;
    let mut stale_rejects = 0;
    let mut guard_blocks = 0;
    let mut interrupts = 0;
    let mut last_latest_tail = String::new();
    let mut last_fanout = String::new();
    let mut last_inflight = String::new();
    let mut last_guard = String::new();
    let mut last_interrupt = String::new();

    for line in lines.into_iter().rev() {
        if line.contains("[latest-tail]") {
            if line.contains("mode=fresh") {
                latest_tail_fresh += 1;
            }
            if line.contains("mode=diff") {
                latest_tail_diff += 1;
            }
            if let Some(bytes) = trace_field_i64(line, "bytes") {
                latest_tail_bytes += bytes;
            }
            last_latest_tail = coordination_trace_line_iso(line);
        }
        if line.contains("jsonl-fanout") {
            fanout_events += 1;
            if line.contains("\"reset\":true") || line.contains("reset=true") {
                fanout_resets += 1;
            }
            last_fanout = coordination_trace_line_iso(line);
        }
        if line.contains("jsonl-broadcast") {
            fanout_subscribers = trace_json_field_i64(line, "subscribers")
                .or_else(|| trace_field_i64(line, "subscribers"));
            last_fanout = coordination_trace_line_iso(line);
        }
        if line.contains("jsonl-cap-trim") {
            cap_trims += 1;
        }
        if line.contains("[inflight-sentinel]") {
            if line.contains("op=write") {
                inflight_writes += 1;
            }
            if line.contains("op=clear") || line.contains("op=stale-startup-clear") {
                inflight_clears += 1;
            }
            last_inflight = coordination_trace_line_iso(line);
        }
        if line.contains("rejected_stale") {
            stale_rejects += 1;
        }
        if line.contains("worklist-guard") || line.contains("[guard]") {
            let lower = line.to_ascii_lowercase();
            if lower.contains("block") || lower.contains("deny") {
                guard_blocks += 1;
            }
            last_guard = coordination_trace_line_iso(line);
        }
        if line.contains("interrupt")
            || line.contains("silence-clear")
            || line.contains("agent-turn-end")
            || line.contains("Esc")
        {
            interrupts += 1;
            last_interrupt = coordination_trace_line_iso(line);
        }
    }

    serde_json::json!({
        "latestTailFresh": latest_tail_fresh,
        "latestTailDiff": latest_tail_diff,
        "latestTailBytes": latest_tail_bytes,
        "fanoutEvents": fanout_events,
        "fanoutResets": fanout_resets,
        "fanoutSubscribers": fanout_subscribers,
        "capTrims": cap_trims,
        "inflightWrites": inflight_writes,
        "inflightClears": inflight_clears,
        "staleRejects": stale_rejects,
        "guardBlocks": guard_blocks,
        "interrupts": interrupts,
        "lastLatestTail": last_latest_tail,
        "lastFanout": last_fanout,
        "lastInflight": last_inflight,
        "lastGuard": last_guard,
        "lastInterrupt": last_interrupt,
    })
}

fn recent_worklist_history<R: tauri::Runtime>(app: &AppHandle<R>) -> Vec<serde_json::Value> {
    let Some(dir) = worklist_history_dir(app) else {
        return Vec::new();
    };
    let mut json_files: Vec<(i64, PathBuf)> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let p = entry.path();
            if p.extension().map_or(false, |e| e == "json") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(ts) = stem.parse::<i64>() {
                        json_files.push((ts, p));
                    }
                }
            }
        }
    }
    json_files.sort_by(|a, b| b.0.cmp(&a.0));
    json_files
        .into_iter()
        .take(5)
        .map(|(ts, json_path)| {
            let md_path = json_path.with_extension("md");
            let changelog = std::fs::read_to_string(&md_path).unwrap_or_default();
            let summary = changelog
                .lines()
                .find(|l| l.starts_with("**Summary:**"))
                .map(|l| l.trim_start_matches("**Summary:**").trim().to_string())
                .unwrap_or_else(|| {
                    if changelog.contains("## Description changed") {
                        "description changed".to_string()
                    } else {
                        "change".to_string()
                    }
                });
            serde_json::json!({
                "ts": ts,
                "iso": format_iso_utc_ms(ts),
                "summary": summary,
            })
        })
        .collect()
}

fn latest_xs_trace_export() -> Option<serde_json::Value> {
    let downloads = home_dir()?.join("Downloads");
    let mut newest: Option<(i64, PathBuf)> = None;
    let read_dir = std::fs::read_dir(downloads).ok()?;
    for entry in read_dir.flatten() {
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("xs-trace-") || !name.ends_with(".json") {
            continue;
        }
        let modified_ms = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if newest.as_ref().map_or(true, |(ts, _)| modified_ms > *ts) {
            newest = Some((modified_ms, p));
        }
    }
    newest.map(|(modified_ms, p)| {
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        serde_json::json!({
            "path": p.to_string_lossy().to_string(),
            "size": size,
            "modifiedAt": modified_ms,
            "modifiedIso": format_iso_utc_ms(modified_ms),
        })
    })
}

fn startup_run_summary(
    trace_text: &str,
    started_at_ms: i64,
    now: i64,
    trace_export: Option<&serde_json::Value>,
) -> serde_json::Value {
    let window_ms = 60_000;
    let end_ms = started_at_ms.saturating_add(window_ms);
    let start_iso = format_iso_utc_ms(started_at_ms);
    let end_iso = format_iso_utc_ms(end_ms);
    let mut latest_tail_requests = 0;
    let mut latest_tail_resets = 0;
    let mut latest_tail_truncations = 0;
    let mut latest_tail_max_body = 0;
    let mut latest_tail_max_content = 0;
    let mut fanout_events = 0;
    let mut fanout_max_len = 0;
    let mut heartbeat_max_drift = 0;
    let mut pty_chunks = 0;
    let mut pty_bytes = 0;
    let mut last_seen = String::new();

    for line in trace_text.lines() {
        let iso = coordination_trace_line_iso(line);
        if iso.is_empty() || iso < start_iso || iso > end_iso {
            continue;
        }
        last_seen = iso;
        if line.contains("path=__sessions/latest-tail") && line.contains("phase=exit") {
            latest_tail_requests += 1;
            if let Some(body_size) = trace_field_i64(line, "body_size") {
                latest_tail_max_body = latest_tail_max_body.max(body_size);
            }
        }
        if line.contains("[latest-tail]") {
            if let Some(bytes) = trace_field_i64(line, "bytes") {
                latest_tail_max_content = latest_tail_max_content.max(bytes);
            }
            if line.contains("truncated=true") {
                latest_tail_truncations += 1;
            }
        }
        if line.contains("jsonl-fanout") {
            fanout_events += 1;
            if line.contains("\"reset\":true") || line.contains("reset=true") {
                latest_tail_resets += 1;
            }
            if let Some(len) =
                trace_json_field_i64(line, "len").or_else(|| trace_field_i64(line, "len"))
            {
                fanout_max_len = fanout_max_len.max(len);
            }
        }
        if line.contains("heartbeat-batch") {
            if let Some(max_drift) = trace_json_field_i64(line, "maxDriftMs") {
                heartbeat_max_drift = heartbeat_max_drift.max(max_drift);
            }
        }
        if line.contains("[pty-in]") {
            pty_chunks += 1;
            if let Some(bytes) = trace_field_i64(line, "bytes") {
                pty_bytes += bytes;
            }
        }
    }

    let trace_export_size = trace_export
        .and_then(|v| v.get("size").and_then(|s| s.as_u64()))
        .unwrap_or(0);
    let trace_export_path = trace_export
        .and_then(|v| v.get("path").and_then(|s| s.as_str()))
        .unwrap_or("");
    let complete = now >= end_ms;
    let level = if latest_tail_max_body > 1_000_000
        || fanout_max_len > 1_000_000
        || heartbeat_max_drift > 1_000
        || trace_export_size > 5_000_000
    {
        "warn"
    } else if latest_tail_requests > 0 || pty_chunks > 0 {
        "ok"
    } else {
        "neutral"
    };

    serde_json::json!({
        "startedAt": format_iso_utc_ms(started_at_ms),
        "windowMs": window_ms,
        "complete": complete,
        "level": level,
        "latestTailRequests": latest_tail_requests,
        "latestTailMaxBody": latest_tail_max_body,
        "latestTailMaxContent": latest_tail_max_content,
        "latestTailResets": latest_tail_resets,
        "latestTailTruncations": latest_tail_truncations,
        "fanoutEvents": fanout_events,
        "fanoutMaxLen": fanout_max_len,
        "heartbeatMaxDrift": heartbeat_max_drift,
        "ptyChunks": pty_chunks,
        "ptyBytes": pty_bytes,
        "traceExportSize": trace_export_size,
        "traceExportPath": trace_export_path,
        "lastSeen": last_seen,
    })
}

fn file_modified_iso(path: &Path) -> String {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format_iso_utc_ms(d.as_millis() as i64))
        .unwrap_or_default()
}

fn command_found(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !stdout.is_empty() {
        Some(stdout)
    } else if !stderr.is_empty() {
        Some(stderr)
    } else {
        Some(cmd.to_string())
    }
}

fn worklist_item_files(item: &serde_json::Value) -> Vec<String> {
    if let Some(files) = item.get("files").and_then(|v| v.as_array()) {
        let collected: Vec<String> = files
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
            .collect();
        if !collected.is_empty() {
            return collected;
        }
    }
    item.get("file")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default()
}

fn git_changed_files(root: &Path) -> HashSet<String> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(root)
        .output();
    let Ok(out) = out else {
        return HashSet::new();
    };
    if !out.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let path = line[3..].trim();
            let path = path.rsplit_once(" -> ").map(|(_, to)| to).unwrap_or(path);
            Some(path.trim_matches('"').to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn authorization_rows<R: tauri::Runtime>(
    app: &AppHandle<R>,
    now: i64,
) -> (Vec<serde_json::Value>, bool, Option<String>) {
    let Some(path) = worklist_auth_file(app) else {
        return (
            vec![
                serde_json::json!({
                    "signal": "Latest record",
                    "level": "neutral",
                    "state": "none",
                    "detail": "No authorization record path",
                    "seen": "",
                }),
                serde_json::json!({
                    "signal": "Record age",
                    "level": "neutral",
                    "state": "none",
                    "detail": "No authorization record path",
                    "seen": "",
                }),
            ],
            false,
            None,
        );
    };
    let modified = file_modified_iso(&path);
    let record: Option<WorklistAuthorizationRecord> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let Some(record) = record else {
        return (
            vec![
                serde_json::json!({
                    "signal": "Latest record",
                    "level": "neutral",
                    "state": "none",
                    "detail": "No readable authorization record",
                    "seen": modified,
                }),
                serde_json::json!({
                    "signal": "Record age",
                    "level": "neutral",
                    "state": "none",
                    "detail": modified,
                    "seen": modified,
                }),
            ],
            false,
            None,
        );
    };
    if record.kind == "none" || record.issued_at_ms <= 0 {
        return (
            vec![
                serde_json::json!({
                    "signal": "Latest record",
                    "level": "neutral",
                    "state": "none",
                    "detail": "No active authorization",
                    "seen": modified,
                }),
                serde_json::json!({
                    "signal": "Record age",
                    "level": "neutral",
                    "state": "none",
                    "detail": modified,
                    "seen": modified,
                }),
            ],
            false,
            None,
        );
    }
    let age_ms = (now - record.issued_at_ms).max(0);
    let pending = record.consumed_at_ms.unwrap_or(0) <= 0;
    let pending_warn = pending && age_ms > 30000;
    let state = if pending {
        format!("pending {}", coordination_duration(age_ms))
    } else {
        format!(
            "consumed {} ago",
            coordination_duration(now - record.consumed_at_ms.unwrap_or(now))
        )
    };
    let detail = format!(
        "{} covering {} items: {}",
        record.kind,
        record.ids.len(),
        record.ids.join(", ").if_empty("none")
    );
    let issue = if pending_warn {
        Some(format!(
            "{} record pending {} without consumer",
            record.kind,
            coordination_duration(age_ms)
        ))
    } else {
        None
    };
    (
        vec![
            serde_json::json!({
                "signal": "Latest record",
                "level": if pending_warn { "warn" } else { "ok" },
                "state": state,
                "detail": detail,
                "seen": format_iso_utc_ms(record.issued_at_ms),
            }),
            serde_json::json!({
                "signal": "Record age",
                "level": if pending_warn { "warn" } else { "ok" },
                "state": coordination_duration(age_ms),
                "detail": modified,
                "seen": modified,
            }),
        ],
        pending_warn,
        issue,
    )
}

// One row per Setup-managed file. Each row shows expected (bundle / marker
// format) vs actual (disk state) so the Status tab can surface stale or
// legacy state without requiring a synthetic verification dance. Mirrors the
// invariants enforced by run_enhance + install_codex_worklist_guard. Refs #174.
fn agent_coordination_rows<R: tauri::Runtime>(app: &AppHandle<R>) -> Vec<serde_json::Value> {
    use serde_json::json;
    let proj = match project_root(Some(app)) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let is_source_repo = proj.join(ENHANCE_SOURCE_BUNDLE_REL).exists();
    let mut rows: Vec<serde_json::Value> = Vec::new();

    // --- Claude sidecar ---
    {
        let path = proj.join(ENHANCE_SIDECAR_REL);
        let seen = file_modified_iso(&path);
        if is_source_repo {
            rows.push(json!({
                "signal": ENHANCE_SIDECAR_REL,
                "level": "neutral",
                "state": "source-repo",
                "detail": "Source repo: bundle is on-disk at app/__shell/conventions.md; Setup does not manage this path here.",
                "seen": seen,
            }));
        } else if !path.exists() {
            rows.push(json!({
                "signal": ENHANCE_SIDECAR_REL,
                "level": "warn",
                "state": "missing",
                "detail": "Sidecar not installed. Run Setup to install agent guidance.",
                "seen": seen,
            }));
        } else {
            let matches = hook_matches_bundle(app, &path, "__shell/conventions.md");
            let disk_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let bundle_len = serve_app_file(Some(app), "__shell/conventions.md")
                .map(|(b, _)| b.len() as u64)
                .unwrap_or(0);
            rows.push(json!({
                "signal": ENHANCE_SIDECAR_REL,
                "level": if matches { "ok" } else { "warn" },
                "state": if matches { "current" } else { "stale" },
                "detail": if matches {
                    format!("Byte-matches bundle ({} B)", bundle_len)
                } else {
                    format!("Disk {} B, bundle {} B — run Setup to refresh", disk_len, bundle_len)
                },
                "seen": seen,
            }));
        }
    }

    // --- CLAUDE.md marker block ---
    {
        let path = proj.join("CLAUDE.md");
        let seen = file_modified_iso(&path);
        if is_source_repo {
            rows.push(json!({
                "signal": "CLAUDE.md (marker block)",
                "level": "neutral",
                "state": "source-repo",
                "detail": "Source repo: CLAUDE.md is hand-maintained; Setup does not write a marker block here.",
                "seen": seen,
            }));
        } else if !path.exists() {
            rows.push(json!({
                "signal": "CLAUDE.md (marker block)",
                "level": "warn",
                "state": "missing",
                "detail": "CLAUDE.md not present. Run Setup to install the @-import.",
                "seen": seen,
            }));
        } else {
            let disk = std::fs::read_to_string(&path).unwrap_or_default();
            let has_new = disk.contains(ENHANCE_MARKER_START);
            let has_legacy = disk.contains(ENHANCE_LEGACY_MARKER_START);
            if has_legacy && !has_new {
                rows.push(json!({
                    "signal": "CLAUDE.md (marker block)",
                    "level": "warn",
                    "state": "legacy-markers",
                    "detail": "Block uses legacy <!-- xmlui-desktop: --> markers. Run Setup to migrate.",
                    "seen": seen,
                }));
            } else if !has_new {
                rows.push(json!({
                    "signal": "CLAUDE.md (marker block)",
                    "level": "warn",
                    "state": "missing",
                    "detail": "CLAUDE.md present but has no Bram marker block.",
                    "seen": seen,
                }));
            } else {
                let expected = format!(
                    "{}\n@{}\n{}",
                    ENHANCE_MARKER_START, ENHANCE_SIDECAR_REL, ENHANCE_MARKER_END
                );
                let block = extract_marker_block(&disk, ENHANCE_MARKER_START, ENHANCE_MARKER_END)
                    .unwrap_or("");
                let matches = block == expected;
                rows.push(json!({
                    "signal": "CLAUDE.md (marker block)",
                    "level": if matches { "ok" } else { "warn" },
                    "state": if matches { "current" } else { "stale" },
                    "detail": if matches {
                        format!("Block matches bundle: @{}", ENHANCE_SIDECAR_REL)
                    } else {
                        "Block content differs from bundle. Run Setup to refresh.".to_string()
                    },
                    "seen": seen,
                }));
            }
        }
    }

    // --- Claude project hook script ---
    {
        let path = proj.join(ENHANCE_HOOK_SCRIPT_REL);
        let seen = file_modified_iso(&path);
        if !path.exists() {
            rows.push(json!({
                "signal": ENHANCE_HOOK_SCRIPT_REL,
                "level": "warn",
                "state": "missing",
                "detail": "Project hook not installed. Run Setup.",
                "seen": seen,
            }));
        } else {
            let matches = hook_matches_bundle(app, &path, ENHANCE_HOOK_BUNDLE_REL);
            let disk_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let bundle_len = serve_app_file(Some(app), ENHANCE_HOOK_BUNDLE_REL)
                .map(|(b, _)| b.len() as u64)
                .unwrap_or(0);
            let detail_when_stale = if is_source_repo {
                format!(
                    "Disk {} B, bundle {} B (source repo: divergence may reflect an in-flight diagnostic edit, not stale Setup)",
                    disk_len, bundle_len
                )
            } else {
                format!(
                    "Disk {} B, bundle {} B — run Setup to refresh",
                    disk_len, bundle_len
                )
            };
            rows.push(json!({
                "signal": ENHANCE_HOOK_SCRIPT_REL,
                "level": if matches { "ok" } else if is_source_repo { "neutral" } else { "warn" },
                "state": if matches { "current" } else { "stale" },
                "detail": if matches {
                    format!("Byte-matches bundle ({} B)", bundle_len)
                } else {
                    detail_when_stale
                },
                "seen": seen,
            }));
        }
    }

    // --- settings.json hook registration ---
    {
        let path = proj.join(ENHANCE_SETTINGS_REL);
        let seen = file_modified_iso(&path);
        let exists = path.exists();
        let registered = settings_has_worklist_guard_hook(&path);
        rows.push(json!({
            "signal": ENHANCE_SETTINGS_REL,
            "level": if registered { "ok" } else { "warn" },
            "state": if registered { "registered" } else if exists { "not-registered" } else { "missing" },
            "detail": if registered {
                "worklist-guard.py registered as PreToolUse hook for Write|Edit"
            } else if exists {
                "settings.json present but worklist-guard.py is not registered"
            } else {
                "settings.json not present"
            },
            "seen": seen,
        }));
    }

    // --- AGENTS.md marker block ---
    {
        let path = proj.join(ENHANCE_CODEX_AGENTS_REL);
        let seen = file_modified_iso(&path);
        if is_source_repo {
            rows.push(json!({
                "signal": "AGENTS.md (marker block)",
                "level": "neutral",
                "state": "source-repo",
                "detail": "Source repo: AGENTS.md is hand-maintained; Setup does not write a marker block here.",
                "seen": seen,
            }));
        } else if !path.exists() {
            rows.push(json!({
                "signal": "AGENTS.md (marker block)",
                "level": "warn",
                "state": "missing",
                "detail": "AGENTS.md not present. Run Setup to install.",
                "seen": seen,
            }));
        } else {
            let disk = std::fs::read_to_string(&path).unwrap_or_default();
            let has_new = disk.contains(ENHANCE_MARKER_START);
            let has_legacy = disk.contains(ENHANCE_LEGACY_MARKER_START);
            if has_legacy && !has_new {
                rows.push(json!({
                    "signal": "AGENTS.md (marker block)",
                    "level": "warn",
                    "state": "legacy-markers",
                    "detail": "Block uses legacy <!-- xmlui-desktop: --> markers. Run Setup to migrate.",
                    "seen": seen,
                }));
            } else if !has_new {
                rows.push(json!({
                    "signal": "AGENTS.md (marker block)",
                    "level": "warn",
                    "state": "missing",
                    "detail": "AGENTS.md present but has no Bram marker block.",
                    "seen": seen,
                }));
            } else {
                let current = codex_agents_block_current(app, &path, is_source_repo);
                rows.push(json!({
                    "signal": "AGENTS.md (marker block)",
                    "level": if current { "ok" } else { "warn" },
                    "state": if current { "current" } else { "stale" },
                    "detail": if current {
                        "Block matches codex-startup-instructions bundle"
                    } else {
                        "Block content differs from bundle. Run Setup to refresh."
                    },
                    "seen": seen,
                }));
            }
        }
    }

    // --- User-global codex hook script ---
    {
        let path_opt = home_dir().map(|h| h.join(ENHANCE_CODEX_HOOK_INSTALL_REL));
        let signal = "~/.bram/codex-worklist-guard.py";
        match path_opt {
            None => rows.push(json!({
                "signal": signal, "level": "warn", "state": "missing",
                "detail": "HOME unset; cannot locate codex hook install path.",
                "seen": "",
            })),
            Some(path) => {
                let seen = file_modified_iso(&path);
                if !path.exists() {
                    rows.push(json!({
                        "signal": signal,
                        "level": "warn",
                        "state": "missing",
                        "detail": "User-global codex hook not installed. Run Setup.",
                        "seen": seen,
                    }));
                } else {
                    let matches = hook_matches_bundle(app, &path, ENHANCE_CODEX_HOOK_BUNDLE_REL);
                    let disk_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let bundle_len = serve_app_file(Some(app), ENHANCE_CODEX_HOOK_BUNDLE_REL)
                        .map(|(b, _)| b.len() as u64)
                        .unwrap_or(0);
                    rows.push(json!({
                        "signal": signal,
                        "level": if matches { "ok" } else { "warn" },
                        "state": if matches { "current" } else { "stale" },
                        "detail": if matches {
                            format!("Byte-matches bundle ({} B)", bundle_len)
                        } else {
                            format!("Disk {} B, bundle {} B — run Setup to refresh", disk_len, bundle_len)
                        },
                        "seen": seen,
                    }));
                }
            }
        }
    }

    // --- ~/.codex/config.toml hook block ---
    {
        let path_opt = home_dir().map(|h| h.join(ENHANCE_CODEX_CONFIG_REL));
        let signal = "~/.codex/config.toml (hook block)";
        match path_opt {
            None => rows.push(json!({
                "signal": signal, "level": "warn", "state": "missing",
                "detail": "HOME unset; cannot locate Codex config.",
                "seen": "",
            })),
            Some(path) => {
                let seen = file_modified_iso(&path);
                if !path.exists() {
                    rows.push(json!({
                        "signal": signal,
                        "level": "warn",
                        "state": "missing",
                        "detail": "Codex config.toml not present.",
                        "seen": seen,
                    }));
                } else {
                    let disk = std::fs::read_to_string(&path).unwrap_or_default();
                    let has_new = disk.contains(ENHANCE_CODEX_TOML_MARKER_START);
                    let has_legacy = disk.contains(ENHANCE_CODEX_LEGACY_TOML_MARKER_START);
                    if has_legacy && !has_new {
                        rows.push(json!({
                            "signal": signal,
                            "level": "warn",
                            "state": "legacy-markers",
                            "detail": "Hook block uses legacy # xmlui-desktop: markers. Run Setup to migrate.",
                            "seen": seen,
                        }));
                    } else if !has_new {
                        rows.push(json!({
                            "signal": signal,
                            "level": "warn",
                            "state": "missing",
                            "detail": "config.toml present but has no Bram hook block. Run Setup to install.",
                            "seen": seen,
                        }));
                    } else {
                        rows.push(json!({
                            "signal": signal,
                            "level": "ok",
                            "state": "current",
                            "detail": "Hook block present with current # bram: markers (content check via worklist-guard-codex.py bundle row)",
                            "seen": seen,
                        }));
                    }
                }
            }
        }
    }

    // --- ~/.codex/config.toml developer_instructions ---
    {
        let path_opt = home_dir().map(|h| h.join(ENHANCE_CODEX_CONFIG_REL));
        let signal = "~/.codex/config.toml (developer_instructions)";
        match path_opt {
            None => rows.push(json!({
                "signal": signal, "level": "warn", "state": "missing",
                "detail": "HOME unset; cannot locate Codex config.",
                "seen": "",
            })),
            Some(path) => {
                let seen = file_modified_iso(&path);
                if !path.exists() {
                    rows.push(json!({
                        "signal": signal,
                        "level": "warn",
                        "state": "missing",
                        "detail": "Codex config.toml not present.",
                        "seen": seen,
                    }));
                } else {
                    let current = codex_instr_block_current(&path);
                    rows.push(json!({
                        "signal": signal,
                        "level": if current { "ok" } else { "warn" },
                        "state": if current { "current" } else { "stale" },
                        "detail": if current {
                            "developer_instructions block matches current gate prose"
                        } else {
                            "developer_instructions absent or stale — run Setup"
                        },
                        "seen": seen,
                    }));
                }
            }
        }
    }

    rows
}

fn coordination_status<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<Vec<u8>, String> {
    let now = unix_now_ms();
    let worklist = worklist_doc(app);
    let items: Vec<serde_json::Value> = worklist
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in &items {
        let status = item
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("proposed")
            .to_string();
        *counts.entry(status).or_insert(0) += 1;
    }
    let applied_count = *counts.get("applied").unwrap_or(&0);
    let proposed_count = *counts.get("proposed").unwrap_or(&0);
    let committed_count = *counts.get("committed").unwrap_or(&0);
    let pruned_count = *counts.get("pruned").unwrap_or(&0);

    let inflight: serde_json::Value = inflight_claim_file(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let claim_ids: Vec<String> = inflight
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let claimed_at = inflight
        .get("claimedAt")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let claim_age_ms = if claimed_at > 0 { now - claimed_at } else { 0 };
    let claim_level = if claim_ids.is_empty() {
        "ok"
    } else if claim_age_ms > 120000 {
        "warn"
    } else {
        "info"
    };

    let trace_text = bram_trace_log_file(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let trace = coordination_trace_summary(&trace_text);
    let history = recent_worklist_history(app);
    let last_history = history
        .first()
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let latest_total = trace["latestTailFresh"].as_i64().unwrap_or(0)
        + trace["latestTailDiff"].as_i64().unwrap_or(0);
    let fresh_heavy = latest_total >= 5
        && trace["latestTailFresh"].as_i64().unwrap_or(0)
            > trace["latestTailDiff"].as_i64().unwrap_or(0);
    let fanout_level = if trace["fanoutEvents"].as_i64().unwrap_or(0) == 0 {
        "neutral"
    } else if trace["fanoutResets"].as_i64().unwrap_or(0) > 3
        || trace["capTrims"].as_i64().unwrap_or(0) > 2
    {
        "warn"
    } else {
        "ok"
    };
    let trace_export = latest_xs_trace_export();
    let startup_run = startup_run_summary(
        &trace_text,
        LOOPBACK_STARTED_MS.get().copied().unwrap_or(now),
        now,
        trace_export.as_ref(),
    );
    if startup_run
        .get("complete")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        && !STARTUP_RUN_TRACE_EMITTED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        append_bram_trace_line(
            app,
            "startup-run",
            &format!(
                "latest_tail_max_body={} fanout_max_len={} heartbeat_max_drift={} pty_chunks={} pty_bytes={} trace_export_size={} level={}",
                startup_run["latestTailMaxBody"].as_i64().unwrap_or(0),
                startup_run["fanoutMaxLen"].as_i64().unwrap_or(0),
                startup_run["heartbeatMaxDrift"].as_i64().unwrap_or(0),
                startup_run["ptyChunks"].as_i64().unwrap_or(0),
                startup_run["ptyBytes"].as_i64().unwrap_or(0),
                startup_run["traceExportSize"].as_u64().unwrap_or(0),
                startup_run["level"].as_str().unwrap_or("neutral"),
            ),
        );
    }
    let trace_export_state = if trace_export.is_some() {
        "found"
    } else {
        "none"
    };
    let trace_export_detail = trace_export
        .as_ref()
        .and_then(|v| v.get("path").and_then(|p| p.as_str()))
        .unwrap_or("No xs-trace export found in ~/Downloads");
    let trace_export_seen = trace_export
        .as_ref()
        .and_then(|v| v.get("modifiedIso").and_then(|p| p.as_str()))
        .unwrap_or("");
    let project_root_path = project_root(Some(app));
    let python_found = if cfg!(windows) {
        command_found("py", &["-3", "--version"])
    } else {
        command_found("which", &["python3"])
    };
    let claude_hook = project_root_path
        .as_ref()
        .map(|p| p.join(ENHANCE_HOOK_SCRIPT_REL));
    let claude_settings = project_root_path
        .as_ref()
        .map(|p| p.join(".claude/settings.json"));
    let claude_hook_exists = claude_hook.as_ref().map_or(false, |p| p.exists());
    let claude_registered = claude_settings
        .as_ref()
        .map_or(false, |p| settings_has_worklist_guard_hook(p));
    let codex_hook = home_dir().map(|p| p.join(ENHANCE_CODEX_HOOK_INSTALL_REL));
    let codex_config = home_dir().map(|p| p.join(".codex/config.toml"));
    let codex_hook_exists = codex_hook.as_ref().map_or(false, |p| p.exists());
    let codex_registered = codex_config
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map_or(false, |s| s.contains("codex-worklist-guard.py"));
    let hooks_rows = vec![
        serde_json::json!({
            "signal": "Python 3",
            "level": if python_found.is_some() { "ok" } else { "warn" },
            "state": if python_found.is_some() { "found" } else { "missing" },
            "detail": python_found.clone().unwrap_or_else(|| "Hooks silently inert - install Python 3".to_string()),
            "seen": "",
        }),
        serde_json::json!({
            "signal": "Claude hook",
            "level": if claude_hook_exists && claude_registered { "ok" } else { "warn" },
            "state": if claude_hook_exists && claude_registered { "registered" } else if claude_hook_exists { "unregistered" } else { "missing" },
            "detail": if !claude_hook_exists { "Hook file missing" } else if !claude_registered { "Hook file present but not registered in settings.json" } else { "Hook file installed and registered" },
            "seen": claude_hook.as_ref().map(|p| file_modified_iso(p)).unwrap_or_default(),
        }),
        serde_json::json!({
            "signal": "Codex hook",
            "level": if codex_hook_exists && codex_registered { "ok" } else { "warn" },
            "state": if codex_hook_exists && codex_registered { "registered" } else if codex_hook_exists { "unregistered" } else { "missing" },
            "detail": if !codex_hook_exists { "Hook file missing" } else if !codex_registered { "Hook file present but not registered in config.toml" } else { "Hook file installed and registered" },
            "seen": codex_hook.as_ref().map(|p| file_modified_iso(p)).unwrap_or_default(),
        }),
    ];
    let applied_items: Vec<&serde_json::Value> = items
        .iter()
        .filter(|item| {
            item.get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("proposed")
                == "applied"
        })
        .collect();
    let changed_files = project_root_path
        .as_ref()
        .map(|p| git_changed_files(p))
        .unwrap_or_default();
    let mut stale_applied = Vec::new();
    let mut matched_applied = 0usize;
    for item in &applied_items {
        let files = worklist_item_files(item);
        let matched = files.iter().any(|f| changed_files.contains(f));
        if matched {
            matched_applied += 1;
        } else if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
            stale_applied.push(id.to_string());
        }
    }
    let applied_integrity_row = if applied_items.is_empty() {
        serde_json::json!({
            "signal": "Applied integrity",
            "level": "neutral",
            "state": "n/a",
            "detail": "No applied items",
            "seen": "",
        })
    } else {
        serde_json::json!({
            "signal": "Applied integrity",
            "level": if stale_applied.is_empty() { "ok" } else { "warn" },
            "state": format!("{}/{} items match working tree", matched_applied, applied_items.len()),
            "detail": if stale_applied.is_empty() { "All applied items have uncommitted changes".to_string() } else { format!("Stale: {}", stale_applied.join(", ")) },
            "seen": "",
        })
    };
    let port_file = project_root_path
        .as_ref()
        .map(|p| p.join("resources/.bram-port"));
    let port_meta_file = port_file.as_ref().map(|p| bram_port_metadata_path(p));
    let bound_port = LOOPBACK_PORT.get().copied();
    let file_port = port_file
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u16>().ok());
    let port_file_exists = port_file.as_ref().map_or(false, |p| p.exists());
    let port_meta: serde_json::Value = port_meta_file
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let meta_port = port_meta
        .get("port")
        .and_then(|v| v.as_u64())
        .and_then(|v| u16::try_from(v).ok());
    let meta_pid = port_meta.get("pid").and_then(|v| v.as_u64());
    let meta_root = port_meta
        .get("projectRoot")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let current_root = project_root_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let meta_started_at = port_meta
        .get("startedAt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let current_pid = std::process::id() as u64;
    let port_mismatch = bound_port.is_some() && file_port.is_some() && bound_port != file_port;
    let meta_mismatch = !port_meta.is_null()
        && (!matches!((file_port, meta_port), (Some(file), Some(meta)) if file == meta)
            || meta_pid.map_or(false, |pid| pid != current_pid)
            || (!meta_root.is_empty() && !current_root.is_empty() && meta_root != current_root));
    let file_port_probe = file_port.map(|file| probe_port_http(file, "/__app-info"));
    let probe_problem = matches!(
        file_port_probe,
        Some(PortStatus::NotListening) | Some(PortStatus::Unresponsive(_))
    );
    let port_level = if port_mismatch || meta_mismatch || probe_problem {
        "warn"
    } else if bound_port.is_some() && file_port.is_some() {
        "ok"
    } else {
        "neutral"
    };
    let port_state = if port_mismatch || meta_mismatch {
        "stale"
    } else {
        match file_port_probe {
            Some(PortStatus::Live) if bound_port.is_some() && file_port.is_some() => "fresh",
            Some(PortStatus::NotListening) => "not listening",
            Some(PortStatus::Unresponsive(_)) => "unresponsive",
            None if port_file_exists => "unreadable",
            None => "missing",
            _ => "unknown",
        }
    };
    let probe_detail = match &file_port_probe {
        Some(PortStatus::Live) => "HTTP responsive".to_string(),
        Some(PortStatus::NotListening) => "port refuses connections".to_string(),
        Some(PortStatus::Unresponsive(reason)) => {
            format!("port accepts TCP but is unresponsive: {}", reason)
        }
        None => "not probed".to_string(),
    };
    let port_row = serde_json::json!({
        "signal": "Port file",
        "level": port_level,
        "state": port_state,
        "detail": match (bound_port, file_port) {
            (Some(bound), Some(file)) => format!(
                "Bound on {}; file reads {}; {}; metadata pid={} port={} root={} started={}",
                bound,
                file,
                probe_detail,
                meta_pid.map(|v| v.to_string()).unwrap_or_else(|| "missing".to_string()),
                meta_port.map(|v| v.to_string()).unwrap_or_else(|| "missing".to_string()),
                if meta_root.is_empty() { "missing" } else { meta_root },
                if meta_started_at.is_empty() { "missing" } else { meta_started_at }
            ),
            (Some(bound), None) => format!("Bound on {}; no readable port file; metadata pid={} started={}", bound, meta_pid.map(|v| v.to_string()).unwrap_or_else(|| "missing".to_string()), if meta_started_at.is_empty() { "missing" } else { meta_started_at }),
            _ => "No bound port available".to_string(),
        },
        "seen": port_file.as_ref().map(|p| file_modified_iso(p)).unwrap_or_default(),
    });
    let loopback_row = serde_json::json!({
        "signal": "Loopback HTTP",
        "level": match &file_port_probe {
            Some(PortStatus::Live) => "ok",
            Some(PortStatus::NotListening) | Some(PortStatus::Unresponsive(_)) => "warn",
            None => "neutral",
        },
        "state": match &file_port_probe {
            Some(PortStatus::Live) => "responsive",
            Some(PortStatus::NotListening) => "refused",
            Some(PortStatus::Unresponsive(_)) => "unresponsive",
            None => "not probed",
        },
        "detail": match &file_port_probe {
            Some(PortStatus::Live) => format!("GET /__app-info succeeded on 127.0.0.1:{}", file_port.unwrap_or_default()),
            Some(PortStatus::NotListening) => format!("GET /__app-info refused on 127.0.0.1:{}", file_port.unwrap_or_default()),
            Some(PortStatus::Unresponsive(reason)) => format!("GET /__app-info failed on 127.0.0.1:{}: {}", file_port.unwrap_or_default(), reason),
            None => "No readable port file to probe".to_string(),
        },
        "seen": format_iso_utc_ms(now),
    });
    let (authorization_rows, orphan_auth, orphan_auth_detail) = authorization_rows(app, now);
    let current_claim_state = if claim_ids.is_empty() {
        "idle".to_string()
    } else {
        format!(
            "{} {}",
            inflight
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("claim"),
            coordination_ago(claimed_at, now)
        )
    };
    let current_claim_detail = if claim_ids.is_empty() {
        "No active spinner sentinel".to_string()
    } else {
        claim_ids.join(", ")
    };
    let completion_monitor = current_turn_completion_monitor();
    let completion_after_claim = if claimed_at > 0 {
        completion_monitor.seen_at_ms >= claimed_at && completion_monitor.claimed_after
    } else {
        completion_monitor.claimed_after
    };
    let completion_state = if completion_monitor.source.is_empty() {
        "none".to_string()
    } else if completion_after_claim {
        format!("{} after claim", completion_monitor.source)
    } else {
        format!("{} before claim", completion_monitor.source)
    };
    let completion_detail = if completion_monitor.source.is_empty() {
        "No turn completion detector decision recorded yet".to_string()
    } else {
        format!(
            "provider {}; reason {}; {}",
            completion_monitor.provider.clone().if_empty("unknown"),
            completion_monitor.reason.clone().if_empty("unknown"),
            completion_monitor.detail
        )
    };
    let trace_pairs_warn = trace["inflightWrites"].as_i64().unwrap_or(0)
        > trace["inflightClears"].as_i64().unwrap_or(0) + 1;
    let stale_reject_warn = trace["staleRejects"].as_i64().unwrap_or(0) > 0;
    let guard_warn = trace["guardBlocks"].as_i64().unwrap_or(0) > 0;
    let interrupt_warn = trace["interrupts"].as_i64().unwrap_or(0) > 0;
    let _ = (orphan_auth, orphan_auth_detail);

    let rows = serde_json::json!({
        "generatedAt": format_iso_utc_ms(now),
        "raw": {
            "worklist": worklist,
            "inflight": inflight,
            "history": history.clone(),
            "trace": trace.clone(),
            "traceExport": trace_export.clone(),
            "startupRun": startup_run.clone(),
            "turnCompletion": {
                "source": completion_monitor.source.clone(),
                "provider": completion_monitor.provider.clone(),
                "reason": completion_monitor.reason.clone(),
                "detail": completion_monitor.detail.clone(),
                "seenAt": if completion_monitor.seen_at_ms > 0 { format_iso_utc_ms(completion_monitor.seen_at_ms) } else { String::new() },
                "claimedAfter": completion_after_claim,
            },
        },
        "sections": [
            {
                "title": "Session",
                "rows": [
                    session_size_status_row(app)
                ]
            },
            {
                "title": "Startup Run",
                "rows": [
                    {
                        "signal": "Payload maxima",
                        "level": startup_run["level"].as_str().unwrap_or("neutral"),
                        "state": if startup_run["complete"].as_bool().unwrap_or(false) { "complete" } else { "collecting" },
                        "detail": format!(
                            "latest-tail body {} KB; content {} KB; fanout {} KB; resets {}; truncations {}",
                            (startup_run["latestTailMaxBody"].as_i64().unwrap_or(0) + 1023) / 1024,
                            (startup_run["latestTailMaxContent"].as_i64().unwrap_or(0) + 1023) / 1024,
                            (startup_run["fanoutMaxLen"].as_i64().unwrap_or(0) + 1023) / 1024,
                            startup_run["latestTailResets"].as_i64().unwrap_or(0),
                            startup_run["latestTailTruncations"].as_i64().unwrap_or(0)
                        ),
                        "seen": startup_run["lastSeen"].as_str().unwrap_or(""),
                    },
                    {
                        "signal": "Renderer drift",
                        "level": if startup_run["heartbeatMaxDrift"].as_i64().unwrap_or(0) > 1000 { "warn" } else if startup_run["heartbeatMaxDrift"].as_i64().unwrap_or(0) > 0 { "ok" } else { "neutral" },
                        "state": format!("{} ms max", startup_run["heartbeatMaxDrift"].as_i64().unwrap_or(0)),
                        "detail": format!(
                            "PTY {} chunks / {} KB over first {}s",
                            startup_run["ptyChunks"].as_i64().unwrap_or(0),
                            (startup_run["ptyBytes"].as_i64().unwrap_or(0) + 1023) / 1024,
                            startup_run["windowMs"].as_i64().unwrap_or(60000) / 1000
                        ),
                        "seen": startup_run["lastSeen"].as_str().unwrap_or(""),
                    },
                    {
                        "signal": "Inspector export",
                        "level": if startup_run["traceExportSize"].as_u64().unwrap_or(0) > 5_000_000 { "warn" } else if startup_run["traceExportSize"].as_u64().unwrap_or(0) > 0 { "ok" } else { "neutral" },
                        "state": format!("{} KB", (startup_run["traceExportSize"].as_u64().unwrap_or(0) + 1023) / 1024),
                        "detail": startup_run["traceExportPath"].as_str().unwrap_or("No xs-trace export found in ~/Downloads"),
                        "seen": trace_export_seen,
                    }
                ]
            },
            {
                "title": "Worklist",
                "rows": [
                    {
                        "signal": "Current items",
                        "level": if applied_count > 0 { "warn" } else { "ok" },
                        "state": format!("{} active", items.len()),
                        "detail": format!("proposed {}, applied {}, committed {}, pruned {}", proposed_count, applied_count, committed_count, pruned_count),
                        "seen": last_history.get("iso").and_then(|v| v.as_str()).unwrap_or(""),
                    },
                    {
                        "signal": "Recent transitions",
                        "level": if history.is_empty() { "neutral" } else { "ok" },
                        "state": format!("{} snapshots", history.len()),
                        "detail": history.iter().filter_map(|h| h.get("summary").and_then(|v| v.as_str())).collect::<Vec<&str>>().join(" | ").if_empty("No worklist history yet"),
                        "seen": last_history.get("iso").and_then(|v| v.as_str()).unwrap_or(""),
                    },
                    applied_integrity_row
                ]
            },
            {
                "title": "Inflight Sentinel",
                "rows": [
                    {
                        "signal": "Current claim",
                        "level": claim_level,
                        "state": current_claim_state,
                        "detail": current_claim_detail,
                        "seen": if claimed_at > 0 { format_iso_utc_ms(claimed_at) } else { String::new() },
                    },
                    {
                        "signal": "Trace pairs",
                        "level": if trace_pairs_warn { "warn" } else { "ok" },
                        "state": format!("{} writes / {} clears", trace["inflightWrites"].as_i64().unwrap_or(0), trace["inflightClears"].as_i64().unwrap_or(0)),
                        "detail": "Recent [inflight-sentinel] records from bram-trace.log",
                        "seen": trace["lastInflight"].as_str().unwrap_or(""),
                    },
                    {
                        "signal": "Turn completion",
                        "level": if claim_ids.is_empty() || completion_after_claim { "ok" } else { "warn" },
                        "state": completion_state,
                        "detail": completion_detail,
                        "seen": if completion_monitor.seen_at_ms > 0 { format_iso_utc_ms(completion_monitor.seen_at_ms) } else { String::new() },
                    },
                    port_row,
                    loopback_row
                ]
            },
            {
                "title": "Hooks",
                "rows": hooks_rows
            },
            {
                "title": "Agent Coordination",
                "rows": agent_coordination_rows(app)
            },
            {
                "title": "Authorization",
                "rows": authorization_rows
            },
            {
                "title": "Latest Tail And Fanout",
                "rows": [
                    {
                        "signal": "latest-tail",
                        "level": if fresh_heavy { "warn" } else if latest_total > 0 { "ok" } else { "neutral" },
                        "state": format!("{} diff / {} fresh", trace["latestTailDiff"].as_i64().unwrap_or(0), trace["latestTailFresh"].as_i64().unwrap_or(0)),
                        "detail": if trace["latestTailBytes"].as_i64().unwrap_or(0) > 0 { format!("{} KB observed in recent trace window", (trace["latestTailBytes"].as_i64().unwrap_or(0) + 1023) / 1024) } else { "No latest-tail trace records in recent window".to_string() },
                        "seen": trace["lastLatestTail"].as_str().unwrap_or(""),
                    },
                    {
                        "signal": "JSONL fanout",
                        "level": fanout_level,
                        "state": format!("{} fanout events", trace["fanoutEvents"].as_i64().unwrap_or(0)),
                        "detail": format!(
                            "resets {}, cap trims {}{}",
                            trace["fanoutResets"].as_i64().unwrap_or(0),
                            trace["capTrims"].as_i64().unwrap_or(0),
                            trace["fanoutSubscribers"].as_i64().map(|n| format!(", subscribers {}", n)).unwrap_or_default()
                        ),
                        "seen": trace["lastFanout"].as_str().unwrap_or(""),
                    }
                ]
            },
            {
                "title": "Guards, Staleness, Interrupts, Traces",
                "rows": [
                    {
                        "signal": "Guard decisions",
                        "level": if guard_warn { "warn" } else { "ok" },
                        "state": format!("{} recent blocks", trace["guardBlocks"].as_i64().unwrap_or(0)),
                        "detail": if trace["guardBlocks"].as_i64().unwrap_or(0) > 0 { "Recent hook block records found in trace" } else { "No recent hook blocks found in trace" },
                        "seen": trace["lastGuard"].as_str().unwrap_or(""),
                    },
                    {
                        "signal": "Stale approvals",
                        "level": if stale_reject_warn { "warn" } else { "ok" },
                        "state": format!("{} rejected stale", trace["staleRejects"].as_i64().unwrap_or(0)),
                        "detail": if trace["staleRejects"].as_i64().unwrap_or(0) > 0 { "Resolve staleness appeared in recent trace" } else { "No rejected_stale resolve records in recent trace" },
                        "seen": "",
                    },
                    {
                        "signal": "Interrupts",
                        "level": if interrupt_warn { "warn" } else { "ok" },
                        "state": format!("{} related records", trace["interrupts"].as_i64().unwrap_or(0)),
                        "detail": if trace["interrupts"].as_i64().unwrap_or(0) > 0 { "Interrupt/silence-clear records appeared recently" } else { "No interrupt-related records in recent trace" },
                        "seen": trace["lastInterrupt"].as_str().unwrap_or(""),
                    },
                    {
                        "signal": "Inspector exports",
                        "level": if trace_export.is_some() { "ok" } else { "neutral" },
                        "state": trace_export_state,
                        "detail": trace_export_detail,
                        "seen": trace_export_seen,
                    }
                ]
            }
        ]
    });

    serde_json::to_vec(&rows).map_err(|e| e.to_string())
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

#[cfg(test)]
mod agent_status_tests {
    use super::{
        parse_claude_natural_end_banner, parse_claude_status_line, parse_claude_status_verb_only,
        should_clear_status_on_turn_activity_stop,
    };

    #[test]
    fn parses_apostrophe_claude_status_verb_full_line() {
        let parsed = parse_claude_status_line("Beboppin'… (12s · thinking more)".as_bytes())
            .expect("status line should parse");

        assert_eq!(parsed.verb, "Beboppin'");
        assert_eq!(parsed.elapsed_text, "12s");
        assert_eq!(parsed.substate.as_deref(), Some("thinking more"));
    }

    #[test]
    fn parses_apostrophe_claude_status_verb_standalone() {
        let verb = parse_claude_status_verb_only("Beboppin'…".as_bytes())
            .expect("standalone status verb should parse");

        assert_eq!(verb, "Beboppin'");
    }

    #[test]
    fn parses_brewed_claude_status_verb_full_line() {
        let parsed =
            parse_claude_status_line("Brewed… (15s)".as_bytes()).expect("status line should parse");

        assert_eq!(parsed.verb, "Brewed");
        assert_eq!(parsed.elapsed_text, "15s");
        assert!(parsed.substate.is_none());
    }

    #[test]
    fn parses_brewed_natural_end_banner() {
        let banner =
            parse_claude_natural_end_banner(b"* Brewed for 30s").expect("banner should parse");

        assert_eq!(banner.verb, "Brewed");
        assert_eq!(banner.duration_text, "30s");
    }

    #[test]
    fn parses_compound_duration_natural_end_banner() {
        let banner =
            parse_claude_natural_end_banner(b"* Worked for 1m 2s").expect("banner should parse");

        assert_eq!(banner.verb, "Worked");
        assert_eq!(banner.duration_text, "1m 2s");
    }

    #[test]
    fn natural_end_banner_takes_rightmost_match() {
        // Older line first, newer second — the newer banner wins.
        let tail = b"* Worked for 5s\n\n* Brewed for 30s\n";
        let banner = parse_claude_natural_end_banner(tail).expect("banner should parse");

        assert_eq!(banner.verb, "Brewed");
        assert_eq!(banner.duration_text, "30s");
    }

    #[test]
    fn natural_end_banner_ignores_prose_for() {
        assert!(parse_claude_natural_end_banner(b"working for the weekend").is_none());
        assert!(parse_claude_natural_end_banner(b"file for review").is_none());
    }

    #[test]
    fn natural_end_banner_ignores_tool_descriptions_with_digit_for() {
        // CC streams tool descriptions like
        // "Searched for 1 pattern, read 1 file" mid-turn. The "for 1"
        // chunk would otherwise look like a banner with duration "1".
        assert!(parse_claude_natural_end_banner(b"Searched for 1 pattern, read 1 file").is_none());
        assert!(parse_claude_natural_end_banner(b"Listed for 5 minutes").is_none());
        assert!(parse_claude_natural_end_banner(b"Brewed for 30 cups").is_none());
    }

    #[test]
    fn parses_duration_with_internal_whitespace_from_csi_g_split() {
        // CC's TUI paints multi-unit durations piecewise:
        // "1m 22s" arrives as "1m 2\x1b[19Gs", and strip_ansi turns
        // the CSI G into a single space, leaving "1m 2 s". The
        // detector should still recognize this as a banner so the
        // row doesn't stay frozen on the prior turn's verb. The
        // reconstructed duration is lossy ("1m 2s" instead of
        // "1m 22s") because the missing "2" lived in the prior
        // screen state we can't recover from the chunk alone.
        let banner = parse_claude_natural_end_banner(b"* Brewed for 1m 2 s")
            .expect("banner should parse despite the strip_ansi-induced split");

        assert_eq!(banner.verb, "Brewed");
        assert_eq!(banner.duration_text, "1m 2s");
    }

    #[test]
    fn short_turn_activity_silence_does_not_clear_status_row() {
        assert!(!should_clear_status_on_turn_activity_stop(
            Some(900),
            false,
            false
        ));
        assert!(!should_clear_status_on_turn_activity_stop(
            Some(3_000),
            false,
            false
        ));
    }

    #[test]
    fn long_turn_activity_silence_can_clear_status_row() {
        assert!(should_clear_status_on_turn_activity_stop(
            Some(10_000),
            false,
            false
        ));
    }

    #[test]
    fn menu_or_post_escape_suppresses_status_clear() {
        assert!(!should_clear_status_on_turn_activity_stop(
            Some(20_000),
            true,
            false
        ));
        assert!(!should_clear_status_on_turn_activity_stop(
            Some(20_000),
            false,
            true
        ));
    }
}

#[cfg(test)]
mod pty_menu_tests {
    use super::{
        extract_codex_command_signature, extract_pending_tool_call_from_jsonl,
        format_pending_tool_call, parse_menu_options, pty_menu_detect,
        pty_menu_input_clears_inflight, pty_output_clears_inflight,
    };
    use serde_json::json;

    #[test]
    fn parses_three_option_claude_menu() {
        let text = "Do you want to use Bash?\n❯1. Yes\n  2. Yes, and don't ask again\n  3. No, and tell agent what to do\n";
        let opts = parse_menu_options(text);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].key, "1");
        assert_eq!(opts[0].label, "Yes");
        assert_eq!(opts[1].key, "2");
        assert_eq!(opts[1].label, "Yes, and don't ask again");
        assert_eq!(opts[2].key, "3");
        assert_eq!(opts[2].label, "No, and tell agent what to do");
    }

    #[test]
    fn pending_tool_call_uses_latest_unresolved_tool_use() {
        let text = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"old","name":"Bash","input":{"command":"ls /tmp"}},{"type":"tool_use","id":"pending","name":"Bash","input":{"command":"touch /tmp/x.txt"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"old","content":"ok"}]}}"#;

        let call = extract_pending_tool_call_from_jsonl(text).expect("pending tool use");
        assert_eq!(format_pending_tool_call(&call), "Bash(touch /tmp/x.txt)");
    }

    #[test]
    fn pending_tool_call_formats_write_and_mcp() {
        let write = super::PendingToolCall {
            name: "Write".to_string(),
            input: json!({"file_path":"/tmp/a.txt","content":"one\ntwo\n"}),
        };
        assert_eq!(
            format_pending_tool_call(&write),
            "Write(/tmp/a.txt)\n  <2 lines, 8 bytes>"
        );

        let mcp = super::PendingToolCall {
            name: "mcp__filesystem__edit_file".to_string(),
            input: json!({"path":"/tmp/a.txt","edits":[{"oldText":"a","newText":"b"}]}),
        };
        assert!(format_pending_tool_call(&mcp).starts_with("filesystem:edit_file("));
    }

    #[test]
    fn pty_menu_equality_treats_signature_appearing_as_visible_state_change() {
        let before = super::PtyMenu {
            tool: "Bash".to_string(),
            text: "Do you want to proceed?".to_string(),
            options: vec![super::MenuOption {
                key: "1".to_string(),
                label: "Yes".to_string(),
            }],
            tool_call_signature: None,
            tool_call_diff: None,
            signature_source: None,
        };
        let after = super::PtyMenu {
            tool_call_signature: Some("Bash(touch /tmp/x.txt)".to_string()),
            ..before.clone()
        };

        assert!(before != after);
    }

    #[test]
    fn parses_two_option_menu() {
        // Real PTY menus emit a blank line between options and the
        // "Esc to cancel · Tab to amend …" footer, which is what the
        // parser uses to know the option list is done.
        let text = "❯ 1. Allow\n  2. Deny\n\nEsc to cancel\n";
        let opts = parse_menu_options(text);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "Allow");
        assert_eq!(opts[1].label, "Deny");
    }

    #[test]
    fn ignores_numbered_permission_text_without_codex_action_required_title() {
        let text = "Permission request\nRun command: cargo test\n\n  1. Allow\n  2. Deny\n\n";

        assert!(pty_menu_detect(text.as_bytes()).is_none());
    }

    #[test]
    fn detects_codex_action_required_menu_with_fragmented_options() {
        let text = "\x1b]0;[ ! ] Action Required | bram\x07\
\nrm -f /Users/jonudell/Desktop/foo.bar\n\
1.\n\
2.\n\
3.\n";
        let menu = pty_menu_detect(text.as_bytes()).expect("codex action required menu");

        assert_eq!(menu.tool, "Bash");
        assert_eq!(
            menu.tool_call_signature.as_deref(),
            Some("Bash(rm -f /Users/jonudell/Desktop/foo.bar)")
        );
        assert_eq!(menu.options.len(), 3);
        assert_eq!(menu.options[0].key, "1");
        assert_eq!(menu.options[0].label, "Option 1");
        assert_eq!(menu.options[1].key, "2");
        assert_eq!(menu.options[1].label, "Option 2");
        assert_eq!(menu.options[2].key, "3");
        assert_eq!(menu.options[2].label, "Option 3");
    }

    #[test]
    fn detects_codex_action_required_menu_when_title_arrives_after_options() {
        let text = "\nrm -f /Users/jonudell/Desktop/foo.bar\n\
1.\n\
2.\n\
3.\n\
\x1b]0;[ ! ] Action Required | bram\x07";
        let menu = pty_menu_detect(text.as_bytes()).expect("codex action required menu");

        assert_eq!(menu.tool, "Bash");
        assert_eq!(
            menu.tool_call_signature.as_deref(),
            Some("Bash(rm -f /Users/jonudell/Desktop/foo.bar)")
        );
        assert_eq!(menu.options.len(), 3);
        assert_eq!(menu.options[2].label, "Option 3");
    }

    #[test]
    fn codex_fragmented_two_option_menu_does_not_invent_third_choice() {
        let text = "\x1b]0;[ ! ] Action Required | bram\x07\
\ncat /Users/jonudell/Desktop/foo.bar\n\
1.\n\
2.\n";
        let menu = pty_menu_detect(text.as_bytes()).expect("codex action required menu");

        assert_eq!(
            menu.tool_call_signature.as_deref(),
            Some("Bash(cat /Users/jonudell/Desktop/foo.bar)")
        );
        assert_eq!(menu.options.len(), 2);
        assert_eq!(menu.options[0].key, "1");
        assert_eq!(menu.options[0].label, "Option 1");
        assert_eq!(menu.options[1].key, "2");
        assert_eq!(menu.options[1].label, "Option 2");
    }

    #[test]
    fn codex_parsed_menu_preserves_cancel_label() {
        let text = "\x1b]0;[ ! ] Action Required | bram\x07\
\n❯ 1. Yes, proceed\n\
  2. No, and tell Codex what to do differently\n\n";
        let menu = pty_menu_detect(text.as_bytes()).expect("codex action required menu");

        assert_eq!(menu.options.len(), 2);
        assert_eq!(menu.tool_call_signature, None);
        assert_eq!(menu.options[1].key, "2");
        assert_eq!(
            menu.options[1].label,
            "No, and tell Codex what to do differently"
        );
    }

    #[test]
    fn codex_command_signature_ignores_prompt_prose_and_options() {
        let text = "Codex wants to run this command:\n\
$ cargo test --manifest-path src-tauri/Cargo.toml\n\
1.\n\
2.\n";

        assert_eq!(
            extract_codex_command_signature(text)
                .as_ref()
                .map(|(_, sig)| sig.as_str()),
            Some("Bash(cargo test --manifest-path src-tauri/Cargo.toml)")
        );
    }

    #[test]
    fn codex_command_signature_prefers_dollar_prompt_in_mashed_text() {
        let text = "Running rm -f /Users/jonudell/Desktop/foo.barWould you like \
to run the following command?Reason:Remove the file.$rm -f \
/Users/jonudell/Desktop/foo.bar› 1. Yes, proceed";

        assert_eq!(
            extract_codex_command_signature(text)
                .as_ref()
                .map(|(_, sig)| sig.as_str()),
            Some("Bash(rm -f /Users/jonudell/Desktop/foo.bar)")
        );
    }

    #[test]
    fn codex_command_signature_extracts_mcp_tool_and_path_from_mashed_text() {
        let text = "AllowthefilesystemMCPservertoruntool\\\"write_file\\\"?content:{\
\\\"nonce\\\":\\\"codex-pending-command-prune-20260604-001\\\",\
\\\"rout...path:/Users/jonudell/bram/resources/.worklist-intent.json> 1. Allow";

        assert_eq!(
            extract_codex_command_signature(text)
                .as_ref()
                .map(|(tool, sig)| (tool.as_str(), sig.as_str())),
            Some((
                "write_file",
                "filesystem:write_file(/Users/jonudell/bram/resources/.worklist-intent.json)"
            ))
        );
    }

    #[test]
    fn codex_command_signature_uses_longest_mcp_path() {
        let text = "filesystem MCP server to run tool \"write_file\"? content: \
path:/Users/ noisy path:/Users/jonudell/Desktop/foo.bar> 1. Yes";

        assert_eq!(
            extract_codex_command_signature(text)
                .as_ref()
                .map(|(tool, sig)| (tool.as_str(), sig.as_str())),
            Some((
                "write_file",
                "filesystem:write_file(/Users/jonudell/Desktop/foo.bar)"
            ))
        );
    }

    #[test]
    fn ignores_visible_action_required_prose_without_osc_title() {
        let text = "The detector should notice Action Required when 1. and 2. are present.\n";

        assert!(pty_menu_detect(text.as_bytes()).is_none());
    }

    #[test]
    fn ignores_stale_worklist_diff_numbered_text() {
        let text = "resources/worklist-drafts/issue.md\n\
diff --git a/src-tauri/src/lib.rs b/src-tauri/src/lib.rs\n\
The Worklist permission panel proposal:\n\
1. Extend pty_menu_detect\n\
2. Reuse parse_menu_options\n";

        assert!(pty_menu_detect(text.as_bytes()).is_none());
    }

    #[test]
    fn ignores_plain_numbered_terminal_output() {
        let text = "Next steps:\n  1. Open the file\n  2. Edit the code\n\n";

        assert!(pty_menu_detect(text.as_bytes()).is_none());
    }

    #[test]
    fn cursor_with_nbsp_space_still_parses() {
        // Newer Claude Code builds emit "❯\u{a0} 1." (cursor + NBSP + space + digit).
        let text = "❯\u{a0} 1. Yes\n  2. No\n";
        let opts = parse_menu_options(text);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].key, "1");
        assert_eq!(opts[1].key, "2");
    }

    #[test]
    fn returns_empty_when_no_numbered_lines() {
        let opts = parse_menu_options("just some preamble text\nwith no options\n");
        assert!(opts.is_empty());
    }

    #[test]
    fn blank_line_terminates_option_collection() {
        // Footer chatter after the blank line is correctly excluded — it
        // doesn't appear as a phantom 3rd option, nor does it get
        // appended to option 2's label.
        let text = "❯1. Yes\n  2. No\n\nEsc to cancel · Tab to amend\n";
        let opts = parse_menu_options(text);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[1].label, "No");
    }

    #[test]
    fn wrapped_label_continues_on_indented_line() {
        // Real-world case from Bash permission: option 2's label wraps to a
        // second visible line in the PTY because it's long.
        let text = "❯ 1. Yes\n  2. Yes, and allow access to tmp/ and touch /tmp/bram-menu-test.txt\n     commands\n  3. No\n";
        let opts = parse_menu_options(text);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].label, "Yes");
        assert!(opts[1].label.starts_with("Yes, and allow access"));
        assert!(opts[1].label.ends_with("commands"));
        assert_eq!(opts[2].label, "No");
    }

    #[test]
    fn wrapped_label_continues_when_indentation_was_stripped() {
        // What actually arrives at the parser after `strip_ansi`: Claude
        // Code renders indentation with cursor-positioning escapes rather
        // than literal spaces, so the continuation line "commands" starts
        // at column 0 in the stripped tail. Verified live during the #169
        // verification pass — captured text snapshot in
        // /tmp/bram-pty-menu-text.txt during the failing run.
        let text = "Doyouwanttoproceed?\n❯1.Yes\n2.Yes,andallowaccesstotmp/andtouch/tmp/bram-menu-test.txt\ncommands\n3.No\n\nEsctocancel·Tabtoamend·ctrl+etoexplain\n";
        let opts = parse_menu_options(text);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].key, "1");
        assert_eq!(opts[0].label, "Yes");
        assert_eq!(opts[1].key, "2");
        assert!(opts[1].label.starts_with("Yes,andallowaccess"));
        assert!(opts[1].label.ends_with("commands"));
        assert_eq!(opts[2].key, "3");
        assert_eq!(opts[2].label, "No");
    }

    #[test]
    fn esc_clears_inflight() {
        assert!(pty_menu_input_clears_inflight("\x1b"));
    }

    #[test]
    fn option_three_clears_inflight() {
        assert!(pty_menu_input_clears_inflight("3\r"));
        assert!(pty_menu_input_clears_inflight("3\n"));
    }

    #[test]
    fn approving_menu_options_do_not_clear_inflight() {
        assert!(!pty_menu_input_clears_inflight("1\r"));
        assert!(!pty_menu_input_clears_inflight("2\r"));
        assert!(!pty_menu_input_clears_inflight("1\n"));
        assert!(!pty_menu_input_clears_inflight("2\n"));
    }

    #[test]
    fn ordinary_typing_does_not_clear_inflight() {
        assert!(!pty_menu_input_clears_inflight(
            "create the file ~/Desktop/foo.bar"
        ));
        assert!(!pty_menu_input_clears_inflight("3"));
        assert!(!pty_menu_input_clears_inflight(""));
    }

    #[test]
    fn codex_cancel_output_clears_inflight() {
        assert!(pty_output_clears_inflight(
            b"\x1b[38;5;1m\xE2\x9C\x97 \x1b[39mYou \x1b[1mcanceled\x1b[22m the request to run \x1b[2mtouch ~/Desktop/foo.bar\x1b[22m"
        ));
        assert!(pty_output_clears_inflight(
            b"Conversation interrupted - tell the model what to do differently."
        ));
    }

    #[test]
    fn ordinary_output_does_not_clear_inflight() {
        assert!(!pty_output_clears_inflight(
            b"Ran touch ~/Desktop/foo.bar\n(no output)"
        ));
    }
}

#[cfg(test)]
mod turn_completion_tests {
    use super::{
        claude_jsonl_completion_decision, codex_jsonl_completion_decision,
        jsonl_completion_provider_for_path,
    };
    use std::path::Path;

    #[test]
    fn claude_end_turn_detects_completion() {
        let content = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[]}}"#;
        let decision = claude_jsonl_completion_decision(content);
        assert!(decision.detected);
        assert_eq!(decision.reason, "end_turn");
    }

    #[test]
    fn claude_non_final_assistant_does_not_clear() {
        let content = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[]}}"#;
        let decision = claude_jsonl_completion_decision(content);
        assert!(!decision.detected);
        assert_eq!(decision.reason, "non-final-assistant");
    }

    #[test]
    fn codex_task_complete_detects_completion() {
        let content = r#"
{"type":"event_msg","payload":{"type":"agent_message","message":"done"}}
{"type":"event_msg","payload":{"type":"token_count"}}
{"type":"event_msg","payload":{"type":"task_complete"}}
"#;
        let decision = codex_jsonl_completion_decision(content);
        assert!(decision.detected);
        assert_eq!(decision.reason, "task_complete");
    }

    #[test]
    fn codex_intermediate_tool_records_do_not_clear() {
        let content = r#"
{"type":"event_msg","payload":{"type":"agent_message","message":"working"}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command"}}
{"type":"event_msg","payload":{"type":"token_count"}}
"#;
        let decision = codex_jsonl_completion_decision(content);
        assert!(!decision.detected);
        assert_eq!(decision.reason, "no-completion-record");
    }

    #[test]
    fn provider_detection_uses_session_roots() {
        assert!(jsonl_completion_provider_for_path(Path::new(
            "/Users/me/.claude/projects/x/session.jsonl"
        ))
        .is_some());
        assert!(jsonl_completion_provider_for_path(Path::new(
            "/Users/me/.codex/sessions/2026/06/01/rollout.jsonl"
        ))
        .is_some());
        assert!(jsonl_completion_provider_for_path(Path::new("/tmp/session.jsonl")).is_none());
    }
}

#[cfg(test)]
mod worklist_doc_tests {
    use super::{
        base_worklist_doc_from_parsed, canonical_item_hash, parse_worklist_draft,
        resolve_worklist_item_draft,
    };
    use serde_json::json;
    use std::fs;

    #[test]
    fn bare_array_root_sets_schema_error() {
        let doc = base_worklist_doc_from_parsed(Some(json!([
            { "id": "x", "file": "foo.txt", "before": "", "after": "" }
        ])));

        assert_eq!(
            doc.get("schemaError").and_then(|v| v.as_str()),
            Some("root-array")
        );
        assert_eq!(doc.get("description").and_then(|v| v.as_str()), Some(""));
        assert_eq!(
            doc.get("items").and_then(|v| v.as_array()).map(|v| v.len()),
            Some(0)
        );
    }

    #[test]
    fn scalar_root_sets_non_object_schema_error() {
        let doc = base_worklist_doc_from_parsed(Some(json!("oops")));

        assert_eq!(
            doc.get("schemaError").and_then(|v| v.as_str()),
            Some("root-non-object")
        );
        assert_eq!(doc.get("description").and_then(|v| v.as_str()), Some(""));
        assert_eq!(
            doc.get("items").and_then(|v| v.as_array()).map(|v| v.len()),
            Some(0)
        );
    }

    #[test]
    fn draft_parser_splits_before_after_sections() {
        let parsed =
            parse_worklist_draft("# Before\n\nold **markdown**\n\n# After\n\nnew `markdown`\n")
                .expect("draft should parse");

        assert_eq!(parsed.0, "old **markdown**");
        assert_eq!(parsed.1, "new `markdown`");
    }

    #[test]
    fn draft_resolver_ignores_inline_and_uses_draft_file() {
        let dir = std::env::temp_dir().join(format!("bram-draft-only-test-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp draft dir");
        fs::write(
            dir.join("legacy-with-inline.md"),
            "# Before\n\ndraft before\n\n# After\n\ndraft after\n",
        )
        .expect("write draft");

        // Legacy item still carries inline keys (shouldn't happen post-guards
        // but verify the merge overrides them with draft content).
        let item = json!({
            "id": "legacy-with-inline",
            "files": ["docs/a.md"],
            "before": "inline before (should be ignored)",
            "after": "inline after (should be ignored)",
        });

        let resolved = resolve_worklist_item_draft(Some(&dir), &item);

        assert_eq!(
            resolved.get("before").and_then(|v| v.as_str()),
            Some("draft before")
        );
        assert_eq!(
            resolved.get("after").and_then(|v| v.as_str()),
            Some("draft after")
        );
        assert_eq!(resolved.get("_draftMissing"), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn draft_resolver_overrides_inline_with_missing_marker_when_no_draft() {
        let item = json!({
            "id": "inline-without-draft",
            "files": ["docs/a.md"],
            "before": "inline before (should be overridden)",
            "after": "inline after (should be overridden)",
        });

        let resolved = resolve_worklist_item_draft(None, &item);

        assert_eq!(resolved.get("before").and_then(|v| v.as_str()), Some(""));
        assert_eq!(resolved.get("after").and_then(|v| v.as_str()), Some(""));
        assert_eq!(
            resolved.get("_draftMissing").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn draft_resolver_loads_metadata_only_item_and_hashes_resolved_content() {
        let dir =
            std::env::temp_dir().join(format!("bram-worklist-draft-test-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp draft dir");
        fs::write(
            dir.join("draft-item.md"),
            "# Before\n\nmetadata only\n\n# After\n\nresolved prose\n",
        )
        .expect("write draft");

        let item = json!({
            "id": "draft-item",
            "status": "proposed",
            "files": ["docs/a.md"],
        });
        let resolved = resolve_worklist_item_draft(Some(&dir), &item);
        let inline_equivalent = json!({
            "id": "draft-item",
            "status": "proposed",
            "files": ["docs/a.md"],
            "before": "metadata only",
            "after": "resolved prose",
        });

        assert_eq!(
            resolved.get("before").and_then(|v| v.as_str()),
            Some("metadata only")
        );
        assert_eq!(
            canonical_item_hash(&resolved),
            canonical_item_hash(&inline_equivalent)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn draft_resolver_marks_missing_draft_explicitly() {
        let item = json!({
            "id": "missing-draft",
            "status": "proposed",
            "files": ["docs/a.md"],
        });

        let resolved = resolve_worklist_item_draft(None, &item);

        assert_eq!(resolved.get("before").and_then(|v| v.as_str()), Some(""));
        assert_eq!(resolved.get("after").and_then(|v| v.as_str()), Some(""));
        assert_eq!(
            resolved.get("_draftMissing").and_then(|v| v.as_bool()),
            Some(true)
        );
    }
}

#[cfg(test)]
mod app_root_resolution_tests {
    use super::bram_app_root_candidates;
    use std::path::PathBuf;

    #[test]
    fn candidates_include_only_bram_owned_locations() {
        let candidates = bram_app_root_candidates(
            Some(PathBuf::from("/bundle/resources")),
            Some(PathBuf::from("/bundle/bin")),
            Some(PathBuf::from("/bundle/bin/bram")),
        );

        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/bundle/resources/app"),
                PathBuf::from("/bundle/bin/app"),
                PathBuf::from("/bundle/bin/app"),
                PathBuf::from("/bundle/bin/../Resources/app"),
            ]
        );
        assert!(candidates
            .iter()
            .all(|p| !p.to_string_lossy().contains("/project/app")));
    }
}

fn init_worklist_file<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<Vec<u8>, String> {
    let path = worklist_file(app).ok_or("could not resolve project root")?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("could not resolve parent for {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {}", parent.display(), e))?;
    if !path.exists() {
        std::fs::write(&path, empty_worklist_json())
            .map_err(|e| format!("write {}: {}", path.display(), e))?;
        if let Ok(mut guard) = last_worklist_cell().lock() {
            *guard = Some(empty_worklist_json().to_string());
        }
    }
    serde_json::to_vec(&worklist_doc(app)).map_err(|e| e.to_string())
}

fn unix_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Howard Hinnant's days-from-civil, inverted. Used by format_iso_utc to
// avoid pulling in chrono just for one timestamp formatter.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 {
        z / 146097
    } else {
        (z - 146096) / 146097
    };
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

fn format_iso_utc(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs.div_euclid(86400);
    let secs_of_day = secs.rem_euclid(86400);
    let h = secs_of_day / 3600;
    let m = secs_of_day % 3600 / 60;
    let s = secs_of_day % 60;
    let (y, mo, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, m, s)
}

// Millisecond-precision variant for bram-trace lines, where sub-second
// alignment with inspector trace `ts` fields matters.
fn format_iso_utc_ms(ms: i64) -> String {
    let secs = ms / 1000;
    let sub = ms.rem_euclid(1000);
    let days = secs.div_euclid(86400);
    let secs_of_day = secs.rem_euclid(86400);
    let h = secs_of_day / 3600;
    let m = secs_of_day % 3600 / 60;
    let s = secs_of_day % 60;
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, h, m, s, sub
    )
}

fn worklist_item_id(item: &serde_json::Value) -> Option<String> {
    item.get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn worklist_item_status(item: &serde_json::Value) -> &str {
    item.get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("proposed")
}

fn worklist_item_str_field<'a>(item: &'a serde_json::Value, key: &str) -> &'a str {
    item.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn worklist_items(doc: &serde_json::Value) -> Vec<serde_json::Value> {
    doc.get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn worklist_description(doc: &serde_json::Value) -> String {
    doc.get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn normalize_turn_submission(data: &str) -> String {
    // toTurn in app/main.js sends `\x15\x1b[200~<text>\x1b[201~\r` — the
    // leading \x15 (NAK, Ctrl-U) clears any pre-existing input line before
    // the bracketed-paste payload arrives. After stripping the bracketed-
    // paste markers, trim any remaining C0 control characters (NAK, CR,
    // LF, etc.) so the structured-line prefix check finds `approved:` /
    // `drop:` at offset 0 instead of `\x15approved:`.
    data.replace("\u{1b}[200~", "")
        .replace("\u{1b}[201~", "")
        .trim_matches(|c: char| c.is_control())
        .trim()
        .to_string()
}

// Pure parser of an `approved:` / `drop:` turn line. Returns the kind
// and the list of (id, optional supplied hash) requests. The hash is
// None for legacy payloads that arrived as full item objects
// (`{items: [<full>]}` or `{ids: [...]}` for old drop), or as plain
// items without a `hash` field.
struct ParsedWorklistAuthorization {
    kind: String,
    requests: Vec<(String, Option<String>, Option<String>)>,
}

fn parse_worklist_authorization_message(text: &str) -> Option<ParsedWorklistAuthorization> {
    let trimmed = text.trim();
    for (prefix, kind) in [("approved:", "approved"), ("drop:", "drop")] {
        let Some(rest) = trimmed.strip_prefix(prefix) else {
            continue;
        };
        let value = serde_json::from_str::<serde_json::Value>(rest.trim()).ok()?;
        let mut requests: Vec<(String, Option<String>, Option<String>)> = Vec::new();
        // Preferred shape (new per-item, plus legacy approve which carried
        // top-level feedback): {items: [{id, hash?, feedback?}, ...]}.
        if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
            for item in items {
                let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let hash = item
                    .get("hash")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let feedback = item
                    .get("feedback")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                requests.push((id.to_string(), hash, feedback));
            }
        }
        // Legacy drop shape: {ids: [...]} — accept if items[] wasn't present.
        if requests.is_empty() {
            if let Some(ids) = value.get("ids").and_then(|v| v.as_array()) {
                for v in ids {
                    if let Some(id) = v.as_str() {
                        requests.push((id.to_string(), None, None));
                    }
                }
            }
        }
        return Some(ParsedWorklistAuthorization {
            kind: kind.to_string(),
            requests,
        });
    }
    None
}

fn build_worklist_authorization_record(
    parsed: ParsedWorklistAuthorization,
    on_disk_items: &[serde_json::Value],
    drafts_dir: Option<&Path>,
    issued_at_ms: i64,
    source: &str,
) -> WorklistAuthorizationRecord {
    let mut ids: Vec<String> = Vec::with_capacity(parsed.requests.len());
    let mut verified_items: Vec<serde_json::Value> = Vec::new();
    let mut mismatched_ids: Vec<String> = Vec::new();
    let mut any_hash_supplied = false;

    for (id, supplied_hash, supplied_feedback) in &parsed.requests {
        ids.push(id.clone());
        let found = on_disk_items.iter().find(|it| {
            it.get("id")
                .and_then(|v| v.as_str())
                .map_or(false, |x| x == id)
        });
        match (supplied_hash, found) {
            (Some(supplied), Some(item)) => {
                any_hash_supplied = true;
                let resolved_item = resolve_worklist_item_draft(drafts_dir, item);
                if &canonical_item_hash(&resolved_item) == supplied {
                    let mut enriched = resolved_item;
                    if let Some(obj) = enriched.as_object_mut() {
                        obj.insert(
                            "feedback".to_string(),
                            serde_json::Value::String(
                                supplied_feedback.clone().unwrap_or_default(),
                            ),
                        );
                    }
                    verified_items.push(enriched);
                } else {
                    mismatched_ids.push(id.clone());
                }
            }
            (Some(_), None) => {
                any_hash_supplied = true;
                mismatched_ids.push(id.clone());
            }
            (None, _) => {
                // Legacy payload: no hash to verify, so no verified item body.
            }
        }
    }

    let kind = if any_hash_supplied && !mismatched_ids.is_empty() {
        "rejected_stale".to_string()
    } else {
        parsed.kind
    };
    let items = if mismatched_ids.is_empty() {
        verified_items
    } else {
        Vec::new()
    };

    WorklistAuthorizationRecord {
        kind,
        ids,
        items,
        mismatched_ids,
        issued_at_ms,
        source: source.to_string(),
        consumed_at_ms: None,
    }
}

fn record_worklist_authorization_from_input<R: tauri::Runtime>(app: &AppHandle<R>, data: &str) {
    let normalized = normalize_turn_submission(data);
    let Some(parsed) = parse_worklist_authorization_message(&normalized) else {
        return;
    };

    // Look up each requested id in the on-disk worklist, recompute its
    // canonical hash, and compare against the supplied hash. Mismatches
    // (or supplied-but-missing items) flip the record to "rejected_stale"
    // so the agent surfaces the staleness rather than acting blind.
    let on_disk_items: Vec<serde_json::Value> = worklist_file(app)
        .and_then(|p| std::fs::read_to_string(&p).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|doc| doc.get("items").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let drafts_dir = worklist_drafts_dir(app);

    let record = build_worklist_authorization_record(
        parsed,
        &on_disk_items,
        drafts_dir.as_deref(),
        unix_now_ms(),
        "pty-write",
    );

    let Some(path) = worklist_auth_file(app) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("[worklist-auth] create {} failed: {}", parent.display(), e);
            return;
        }
    }
    // Detect clobber: if a prior, not-yet-consumed record exists with a
    // different kind, the new write overwrites it. Read the file before
    // serializing the new record so the prior_kind lookup doesn't race
    // with our own write.
    if bram_trace_enabled() {
        let prior_kind = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<WorklistAuthorizationRecord>(&s).ok())
            .and_then(|prior| {
                if prior.consumed_at_ms.is_some() {
                    None
                } else if prior.kind == record.kind {
                    None
                } else {
                    Some(prior.kind)
                }
            });
        let op = if prior_kind.is_some() {
            "clobber"
        } else {
            "write"
        };
        let prior_field = prior_kind
            .as_deref()
            .map(|k| format!(" prior_kind={}", k))
            .unwrap_or_default();
        append_bram_trace_line(
            app,
            "auth-record",
            &format!(
                "op={} kind={} ids={}{} source={}",
                op,
                record.kind,
                serde_json::to_string(&record.ids).unwrap_or_else(|_| "[]".to_string()),
                prior_field,
                record.source
            ),
        );
    }
    let body = match serde_json::to_string_pretty(&record) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[worklist-auth] serialize failed: {}", e);
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, format!("{}\n", body)) {
        eprintln!("[worklist-auth] write {} failed: {}", path.display(), e);
    }
}

fn read_active_worklist_authorization<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Option<WorklistAuthorizationRecord> {
    let path = worklist_auth_file(app)?;
    let content = std::fs::read_to_string(path).ok()?;
    let record = serde_json::from_str::<WorklistAuthorizationRecord>(&content).ok()?;
    if record.consumed_at_ms.is_some() {
        return None;
    }
    match record.kind.as_str() {
        "approved" | "drop" => Some(record),
        _ => None,
    }
}

fn consume_worklist_authorization<R: tauri::Runtime>(app: &AppHandle<R>) {
    let Some(path) = worklist_auth_file(app) else {
        return;
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut record = match serde_json::from_str::<WorklistAuthorizationRecord>(&content) {
        Ok(r) => r,
        Err(_) => return,
    };
    if record.consumed_at_ms.is_some() {
        return;
    }
    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "auth-record",
            &format!(
                "op=consume kind={} ids={}",
                record.kind,
                serde_json::to_string(&record.ids).unwrap_or_else(|_| "[]".to_string())
            ),
        );
    }
    record.consumed_at_ms = Some(unix_now_ms());
    let body = match serde_json::to_string_pretty(&record) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = std::fs::write(&path, format!("{}\n", body));
}

// Pure classification of proposed-item removals between two worklist
// snapshots: which are authorized drops vs. unauthorized removals the watcher
// must revert. Extracted from maybe_enforce_worklist_policy so the revert
// decision is unit-testable without an AppHandle. Returns
// (dropped_via_auth, violations), violations as (id, status).
fn classify_worklist_removals(
    prior_items: &[serde_json::Value],
    current_items: &[serde_json::Value],
    auth_kind: Option<&str>,
    auth_ids: &std::collections::HashSet<String>,
) -> (Vec<String>, Vec<(String, String)>) {
    let current_ids: std::collections::HashSet<String> =
        current_items.iter().filter_map(worklist_item_id).collect();
    let mut dropped_via_auth: Vec<String> = Vec::new();
    let mut violations: Vec<(String, String)> = Vec::new();
    for item in prior_items {
        let Some(id) = worklist_item_id(item) else {
            continue;
        };
        if current_ids.contains(&id) {
            continue;
        }
        let status = worklist_item_status(item).to_string();
        if status == "applied" {
            continue;
        }
        if auth_kind == Some("drop") && auth_ids.contains(&id) {
            dropped_via_auth.push(id);
            continue;
        }
        violations.push((id, status));
    }
    (dropped_via_auth, violations)
}

fn maybe_enforce_worklist_policy<R: tauri::Runtime>(
    app: &AppHandle<R>,
    prior_str: &str,
    current_str: &str,
) -> bool {
    let prior_doc: serde_json::Value = serde_json::from_str(prior_str).unwrap_or_default();
    let current_doc: serde_json::Value = serde_json::from_str(current_str).unwrap_or_default();
    let prior_items = worklist_items(&prior_doc);
    let current_items = worklist_items(&current_doc);
    let auth = read_active_worklist_authorization(app);
    let auth_ids: std::collections::HashSet<String> = auth
        .as_ref()
        .map(|record| record.ids.iter().cloned().collect())
        .unwrap_or_default();
    let (dropped_via_auth, violations) = classify_worklist_removals(
        &prior_items,
        &current_items,
        auth.as_ref().map(|a| a.kind.as_str()),
        &auth_ids,
    );

    if violations.is_empty() {
        if !dropped_via_auth.is_empty() {
            // Agent-path-symmetry fix: when an agent (Codex) edits
            // worklist.json directly to prune a drop-authorized item
            // instead of going through /__worklist/resolve +
            // /__worklist/mutate, no inflight sentinel write/clear
            // fires and the iframe's `submitting=true` (set on click)
            // never gets cleared — the Worklist tab becomes
            // unselectable. Mirror what /resolve + /mutate would have
            // emitted: write the sentinel, then immediately clear it.
            // The two inflight-claim-changed events drive the iframe's
            // DataSource to refetch /__inflight, find no claim, and
            // clear local submitting state. Same outcome as the
            // Claude path; symmetric across agents.
            //
            // Harmless when invoked via the Claude path too — by the
            // time the policy validator runs after /mutate, the
            // sentinel has already been cleared, so write+clear here
            // is a small redundant pair of events. No state
            // divergence.
            write_inflight_claim_sentinel(app, &dropped_via_auth, "drop");
            clear_inflight_claim_sentinel(app, &dropped_via_auth);
            consume_worklist_authorization(app);
        }
        return true;
    }

    let Some(path) = worklist_file(app) else {
        return false;
    };
    let bad = violations
        .iter()
        .map(|(id, status)| format!("\"{}\" (status={})", id, status))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "[worklist-enforce] reverting unauthorized removal of {} via watcher fallback; last auth kind={}",
        bad,
        auth.as_ref().map(|a| a.kind.as_str()).unwrap_or("none")
    );
    if let Err(e) = std::fs::write(&path, prior_str) {
        eprintln!(
            "[worklist-enforce] failed to restore {}: {}",
            path.display(),
            e
        );
        return false;
    }
    false
}

// Computes the diff between two worklist snapshots and renders it as a
// markdown changelog. Returns None when no meaningful change is detected
// (no items proposed/advanced/renamed/pruned and description unchanged),
// so the caller can suppress trivial snapshots.
// Classify each diffed worklist change by the lifecycle phase it represents:
//   proposed:  new id introduced (status="proposed" or unset)
//   applied:   existing id transitioned status (was proposed, now applied)
//   committed: id removed AND auth record says kind="approved" for that id
//   dropped:   id removed AND auth record says kind="drop" for that id
// The 'renamed' and 'pruned' buckets from earlier versions were removed:
// rename_from was retired in 67eff19, and unauthorized prunes are already
// blocked by maybe_enforce_worklist_policy before they can reach the watcher,
// so every removal flows through either the approved or drop channel.
fn generate_worklist_changelog<R: tauri::Runtime>(
    app: &AppHandle<R>,
    prior: &serde_json::Value,
    current: &serde_json::Value,
    ts_ms: i64,
) -> Option<String> {
    let prior_items = worklist_items(prior);
    let current_items = worklist_items(current);
    let prior_by_id: HashMap<String, &serde_json::Value> = prior_items
        .iter()
        .filter_map(|i| worklist_item_id(i).map(|id| (id, i)))
        .collect();
    let current_by_id: HashMap<String, &serde_json::Value> = current_items
        .iter()
        .filter_map(|i| worklist_item_id(i).map(|id| (id, i)))
        .collect();

    // Read the auth record once so we can classify removals as committed,
    // dropped, or pruned. Failure to load returns None → all removals fall
    // back to the 'pruned' bucket.
    let auth: Option<WorklistAuthorizationRecord> = worklist_auth_file(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok());

    let mut proposed: Vec<&serde_json::Value> = Vec::new();
    let mut applied: Vec<(&serde_json::Value, String, String)> = Vec::new();

    for item in &current_items {
        let id = match worklist_item_id(item) {
            Some(id) => id,
            None => continue,
        };
        match prior_by_id.get(&id) {
            Some(prev) => {
                let prev_status = worklist_item_status(prev).to_string();
                let new_status = worklist_item_status(item).to_string();
                if prev_status != new_status {
                    applied.push((item, prev_status, new_status));
                }
            }
            None => {
                proposed.push(item);
            }
        }
    }

    let mut committed: Vec<&serde_json::Value> = Vec::new();
    let mut dropped: Vec<&serde_json::Value> = Vec::new();
    for item in &prior_items {
        let id = match worklist_item_id(item) {
            Some(id) => id,
            None => continue,
        };
        if current_by_id.contains_key(&id) {
            continue;
        }
        // Removal classification, in order:
        //   1. Prior status == "applied" → committed unconditionally. Matches
        //      the maybe_enforce_worklist_policy hook, which lets applied-status
        //      items prune freely (commit-then-prune is legitimate, no fresh
        //      auth required). Without this branch, a commit-prune whose auth
        //      record was overwritten between approve and prune (any subsequent
        //      drop:/approved: payload replaces the single-slot record) falls
        //      out of both buckets and the history UI shows "change".
        //   2. auth.kind == "drop" and id in auth.ids → dropped (user pruned
        //      a still-proposed item via the drop authorization channel).
        //   3. auth.kind == "approved" and id in auth.ids → committed (unusual:
        //      a proposed item was removed via approved auth without first
        //      transitioning to applied — included for completeness).
        //   4. else → skip silently; the policy hook would have reverted any
        //      other case before the watcher saw it.
        let prior_status = worklist_item_status(item).to_string();
        if prior_status == "applied" {
            committed.push(item);
            continue;
        }
        if let Some(rec) = auth.as_ref() {
            if rec.ids.iter().any(|i| i == &id) {
                match rec.kind.as_str() {
                    "drop" => dropped.push(item),
                    "approved" => committed.push(item),
                    _ => {}
                }
            }
        }
    }

    let prior_desc = worklist_description(prior);
    let current_desc = worklist_description(current);
    let description_changed = prior_desc != current_desc;

    // Suppress only when nothing meaningful happened. committed/dropped ARE
    // meaningful (they mark lifecycle endpoints), so a clean prune-on-commit
    // sequence now leaves a history entry instead of being silently skipped.
    if !description_changed
        && proposed.is_empty()
        && applied.is_empty()
        && committed.is_empty()
        && dropped.is_empty()
    {
        return None;
    }

    let mut out = String::new();
    out.push_str(&format!(
        "# Worklist change @ {} ({})\n\n",
        format_iso_utc(ts_ms),
        ts_ms
    ));
    let mut tallies: Vec<String> = Vec::new();
    if !proposed.is_empty() {
        tallies.push(format!("{} proposed", proposed.len()));
    }
    if !applied.is_empty() {
        tallies.push(format!("{} applied", applied.len()));
    }
    if !committed.is_empty() {
        tallies.push(format!("{} committed", committed.len()));
    }
    if !dropped.is_empty() {
        tallies.push(format!("{} dropped", dropped.len()));
    }
    if !tallies.is_empty() {
        out.push_str(&format!("**Summary:** {}\n\n", tallies.join(", ")));
    }

    if description_changed {
        out.push_str("## Description changed\n\n");
        out.push_str(&format!(
            "Was: `{}`\nNow: `{}`\n\n",
            prior_desc, current_desc
        ));
    }

    if !proposed.is_empty() {
        out.push_str("## Items proposed\n\n");
        for item in &proposed {
            let id = worklist_item_id(item).unwrap_or_default();
            out.push_str(&format!(
                "- `{}` ({}, `{}`)\n",
                id,
                worklist_item_status(item),
                worklist_item_str_field(item, "file")
            ));
            let before = worklist_item_str_field(item, "before");
            if !before.is_empty() {
                out.push_str(&format!("  - **Before:** {}\n", before.replace('\n', " ")));
            }
            let after = worklist_item_str_field(item, "after");
            if !after.is_empty() {
                out.push_str(&format!("  - **After:** {}\n", after.replace('\n', " ")));
            }
        }
        out.push('\n');
    }

    if !applied.is_empty() {
        out.push_str("## Items applied\n\n");
        for (item, from, to) in &applied {
            let id = worklist_item_id(item).unwrap_or_default();
            out.push_str(&format!("- `{}`: {} → {}\n", id, from, to));
        }
        out.push('\n');
    }

    let emit_removed_section = |out: &mut String, header: &str, items: &[&serde_json::Value]| {
        if items.is_empty() {
            return;
        }
        out.push_str(&format!("## {}\n\n", header));
        for item in items {
            let id = worklist_item_id(item).unwrap_or_default();
            out.push_str(&format!(
                "- `{}` (was {}, `{}`)\n",
                id,
                worklist_item_status(item),
                worklist_item_str_field(item, "file")
            ));
            let before = worklist_item_str_field(item, "before");
            if !before.is_empty() {
                out.push_str(&format!("  - **Before:** {}\n", before.replace('\n', " ")));
            }
            let after = worklist_item_str_field(item, "after");
            if !after.is_empty() {
                out.push_str(&format!("  - **After:** {}\n", after.replace('\n', " ")));
            }
        }
        out.push('\n');
    };
    emit_removed_section(&mut out, "Items committed", &committed);
    emit_removed_section(&mut out, "Items dropped", &dropped);

    // Trailing-newline padding kept from the legacy bottom of the function.
    {
        out.push('\n');
    }

    Some(out)
}

// Called from the watcher when resources/worklist.json changes. Reads the
// current file, compares to the cached prior contents, and if different
// writes the *prior* contents to worklist-history/<unix_ms>.json plus a
// changelog .md. Best-effort: errors here must not break the underlying
// worklist write, which has already completed.
fn maybe_snapshot_worklist<R: tauri::Runtime>(app: &AppHandle<R>) {
    let Some(file) = worklist_file(app) else {
        return;
    };
    let Some(history_dir) = worklist_history_dir(app) else {
        return;
    };
    let current_raw_str = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(_) => return,
    };
    let current_raw_doc: serde_json::Value =
        serde_json::from_str(&current_raw_str).unwrap_or_default();
    let drafts_dir = worklist_drafts_dir(app);
    let current_doc = resolve_worklist_doc_drafts(drafts_dir.as_deref(), &current_raw_doc);
    let current_effective_str = serde_json::to_string_pretty(&current_doc)
        .map(|s| format!("{}\n", s))
        .unwrap_or_else(|_| current_raw_str.clone());

    {
        let cell = last_worklist_cell();
        let mut guard = match cell.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        match guard.clone() {
            Some(prior_raw_str) => {
                if prior_raw_str != current_raw_str {
                    if !maybe_enforce_worklist_policy(app, &prior_raw_str, &current_raw_str) {
                        return;
                    }
                    // Raw cache is for authorization enforcement only. History
                    // below uses the separately resolved/effective cache.
                    *guard = Some(current_raw_str.clone());
                }
            }
            None => {
                // First raw observation: seed both caches, no snapshot.
                *guard = Some(current_raw_str);
                if let Ok(mut effective_guard) = last_worklist_effective_cell().lock() {
                    *effective_guard = Some(current_effective_str);
                }
                return;
            }
        }
    }

    let prior_effective_str = {
        let cell = last_worklist_effective_cell();
        let mut guard = match cell.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let prior = match guard.clone() {
            Some(s) => s,
            None => {
                *guard = Some(current_effective_str);
                return;
            }
        };
        if prior == current_effective_str {
            return;
        }
        // Always update the effective cache so the next history diff sees the
        // latest resolved prose, even when this write is suppressed below.
        *guard = Some(current_effective_str.clone());
        prior
    };

    let ts = unix_now_ms();
    let prior_doc: serde_json::Value =
        serde_json::from_str(&prior_effective_str).unwrap_or_default();
    let changelog = match generate_worklist_changelog(app, &prior_doc, &current_doc, ts) {
        Some(s) => s,
        None => {
            eprintln!("[worklist-history] suppressed trivial change @ {}", ts);
            return;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&history_dir) {
        eprintln!("[worklist-history] create_dir_all failed: {}", e);
        return;
    }
    // Snapshot the POST-change state — each snapshot is a checkpoint of
    // the worklist as it stands at that moment. The .md changelog
    // describes the transition from the prior checkpoint.
    let snapshot_path = history_dir.join(format!("{}.json", ts));
    if let Err(e) = std::fs::write(&snapshot_path, &current_effective_str) {
        eprintln!("[worklist-history] write snapshot failed: {}", e);
    }
    let changelog_path = history_dir.join(format!("{}.md", ts));
    if let Err(e) = std::fs::write(&changelog_path, changelog) {
        eprintln!("[worklist-history] write changelog failed: {}", e);
    }
    eprintln!(
        "[worklist-history] snapshot @ {} ({} bytes)",
        ts,
        current_effective_str.len()
    );
}

fn init_worklist_cache<R: tauri::Runtime>(app: &AppHandle<R>) {
    let Some(file) = worklist_file(app) else {
        return;
    };
    if let Ok(s) = std::fs::read_to_string(&file) {
        if let Ok(mut guard) = last_worklist_cell().lock() {
            *guard = Some(s.clone());
        }
        let doc: serde_json::Value = serde_json::from_str(&s).unwrap_or_default();
        let drafts_dir = worklist_drafts_dir(app);
        let effective_doc = resolve_worklist_doc_drafts(drafts_dir.as_deref(), &doc);
        if let Ok(mut guard) = last_worklist_effective_cell().lock() {
            *guard = serde_json::to_string_pretty(&effective_doc)
                .ok()
                .map(|body| format!("{}\n", body));
        }
    }
}

fn cap_history_diff(diff: &str) -> String {
    cap_diff(diff, HISTORY_DIFF_MAX_LINES, HISTORY_DIFF_MAX_BYTES)
}

fn cap_diff(diff: &str, max_lines: usize, max_bytes: usize) -> String {
    if diff.len() <= max_bytes && diff.lines().count() <= max_lines {
        return diff.to_string();
    }
    let mut out = String::new();
    let mut bytes = 0usize;
    let mut emitted = 0usize;
    let mut total_lines = 0usize;
    for line in diff.lines() {
        total_lines += 1;
        if emitted >= max_lines || bytes + line.len() + 1 > max_bytes {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            bytes += 1;
        }
        out.push_str(line);
        bytes += line.len();
        emitted += 1;
    }
    let omitted_lines = total_lines.saturating_sub(emitted);
    let omitted_bytes = diff.len().saturating_sub(bytes);
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!(
        "... diff truncated: {} lines / {} bytes omitted",
        omitted_lines, omitted_bytes
    ));
    out
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WorklistHistoryPhase {
    ts: i64,
    iso: String,
    summary: String,
    summary_label: String,
    full_changelog: String,
    changelog: String,
    diff: String,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WorklistHistoryGroup {
    id: String,
    title: String,
    ids: Vec<String>,
    phases: Vec<WorklistHistoryPhase>,
    latest_ts: i64,
    latest_iso: String,
    phase_count: usize,
    subtitle: String,
    kind: String,
    current_item: Option<serde_json::Value>,
    prose_phase_summary: String,
}

fn history_compact_iso(iso: &str) -> String {
    if iso.len() >= 16 {
        format!("{} {}", &iso[8..10], &iso[11..16])
    } else {
        iso.to_string()
    }
}

fn history_compact_range(first_iso: &str, last_iso: &str) -> String {
    if first_iso.len() >= 16 && last_iso.len() >= 16 && first_iso.get(0..7) == last_iso.get(0..7) {
        if first_iso.get(0..10) == last_iso.get(0..10) {
            format!(
                "{} {} -> {}",
                &first_iso[8..10],
                &first_iso[11..16],
                &last_iso[11..16]
            )
        } else {
            format!(
                "{} -> {}",
                history_compact_iso(first_iso),
                history_compact_iso(last_iso)
            )
        }
    } else {
        format!(
            "{} -> {}",
            first_iso.chars().take(16).collect::<String>(),
            last_iso.chars().take(16).collect::<String>()
        )
    }
}

fn worklist_history_summary(changelog: &str) -> String {
    changelog
        .lines()
        .find(|l| l.starts_with("**Summary:**"))
        .map(|l| l.trim_start_matches("**Summary:**").trim().to_string())
        .unwrap_or_else(|| {
            if changelog.contains("## Description changed") {
                String::from("description changed")
            } else {
                String::from("change")
            }
        })
}

fn worklist_history_ids(changelog: &str, doc: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for line in changelog.lines() {
        if !line.starts_with("- `") {
            continue;
        }
        let rest = &line[3..];
        if let Some(end) = rest.find('`') {
            let after = &rest[end + 1..];
            let looks_like_item = after.starts_with(" (was ")
                || after.starts_with(" (proposed")
                || after.starts_with(" (applied")
                || after.starts_with(" (committed")
                || after.starts_with(" (dropped")
                || after.starts_with(": proposed")
                || after.starts_with(": applied")
                || after.starts_with(": committed")
                || after.starts_with(": dropped");
            if looks_like_item {
                ids.push(rest[..end].to_string());
            }
        }
    }
    if ids.is_empty() {
        if let Some(items) = doc.get("items").and_then(|v| v.as_array()) {
            for item in items {
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    ids.push(id.to_string());
                }
            }
        }
    }
    ids
}

fn worklist_history_item_state(doc: &serde_json::Value, id: &str) -> Option<serde_json::Value> {
    doc.get("items")
        .and_then(|v| v.as_array())
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("id").and_then(|v| v.as_str()) == Some(id))
                .cloned()
        })
}

fn worklist_history_json_line(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("null"))
}

fn worklist_history_item_diff(
    before: Option<&serde_json::Value>,
    after: Option<&serde_json::Value>,
) -> String {
    match (before, after) {
        (None, None) => String::new(),
        (None, Some(item)) => serde_json::to_string_pretty(item)
            .unwrap_or_else(|_| worklist_history_json_line(item))
            .lines()
            .map(|line| format!("+ {}", line))
            .collect::<Vec<String>>()
            .join("\n"),
        (Some(item), None) => serde_json::to_string_pretty(item)
            .unwrap_or_else(|_| worklist_history_json_line(item))
            .lines()
            .map(|line| format!("- {}", line))
            .collect::<Vec<String>>()
            .join("\n"),
        (Some(prev), Some(next)) => {
            if prev == next {
                return String::from("No item data changed.");
            }
            let Some(prev_obj) = prev.as_object() else {
                return format!(
                    "- {}\n+ {}",
                    worklist_history_json_line(prev),
                    worklist_history_json_line(next)
                );
            };
            let Some(next_obj) = next.as_object() else {
                return format!(
                    "- {}\n+ {}",
                    worklist_history_json_line(prev),
                    worklist_history_json_line(next)
                );
            };
            let mut keys: BTreeSet<String> = BTreeSet::new();
            for key in prev_obj.keys() {
                keys.insert(key.to_string());
            }
            for key in next_obj.keys() {
                keys.insert(key.to_string());
            }
            let mut lines: Vec<String> = Vec::new();
            for key in keys {
                let old = prev_obj.get(&key);
                let new = next_obj.get(&key);
                if old == new {
                    continue;
                }
                if let Some(v) = old {
                    lines.push(format!("- \"{}\": {}", key, worklist_history_json_line(v)));
                }
                if let Some(v) = new {
                    lines.push(format!("+ \"{}\": {}", key, worklist_history_json_line(v)));
                }
            }
            if lines.is_empty() {
                String::from("No item data changed.")
            } else {
                lines.join("\n")
            }
        }
    }
}

fn recent_worklist_history_groups<R: tauri::Runtime>(
    app: &AppHandle<R>,
    limit: usize,
) -> Vec<WorklistHistoryGroup> {
    let Some(dir) = worklist_history_dir(app) else {
        return Vec::new();
    };
    let mut json_files: Vec<(i64, PathBuf)> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let p = entry.path();
            if p.extension().map_or(false, |e| e == "json") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(ts) = stem.parse::<i64>() {
                        json_files.push((ts, p));
                    }
                }
            }
        }
    }
    json_files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut groups: Vec<WorklistHistoryGroup> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut last_state: HashMap<String, serde_json::Value> = HashMap::new();

    for (ts, json_path) in json_files {
        let raw = std::fs::read_to_string(&json_path).unwrap_or_default();
        let doc = serde_json::from_str::<serde_json::Value>(&raw).unwrap_or_default();
        let md_path = json_path.with_extension("md");
        let changelog = std::fs::read_to_string(&md_path).unwrap_or_default();
        let summary = worklist_history_summary(&changelog);
        let ids = worklist_history_ids(&changelog, &doc);
        let iso = format_iso_utc(ts);

        if ids.len() == 1 {
            let id = ids[0].clone();
            let current = worklist_history_item_state(&doc, &id);
            let previous = last_state.get(&id);
            let diff = worklist_history_item_diff(previous, current.as_ref());
            let display_item = current.clone().or_else(|| previous.cloned());
            match current {
                Some(item) => {
                    last_state.insert(id.clone(), item);
                }
                None => {
                    last_state.remove(&id);
                }
            }
            let phase = WorklistHistoryPhase {
                ts,
                iso: iso.clone(),
                summary: summary.clone(),
                summary_label: format!(
                    "{} · {}",
                    iso.chars().take(16).collect::<String>(),
                    summary
                ),
                full_changelog: String::new(),
                changelog: String::new(),
                diff: cap_history_diff(&diff),
            };
            let group_idx = match by_id.get(&id).copied() {
                Some(idx) => idx,
                None => {
                    let idx = groups.len();
                    by_id.insert(id.clone(), idx);
                    groups.push(WorklistHistoryGroup {
                        id: id.clone(),
                        title: id.clone(),
                        ids: vec![id.clone()],
                        phases: Vec::new(),
                        latest_ts: 0,
                        latest_iso: String::new(),
                        phase_count: 0,
                        subtitle: String::new(),
                        kind: String::from("item"),
                        current_item: None,
                        prose_phase_summary: String::new(),
                    });
                    idx
                }
            };
            if let Some(group) = groups.get_mut(group_idx) {
                group.latest_ts = ts;
                group.latest_iso = iso;
                group.phases.push(phase);
                group.current_item = display_item;
                group.prose_phase_summary = summary;
            }
        } else {
            let phase = WorklistHistoryPhase {
                ts,
                iso: iso.clone(),
                summary: summary.clone(),
                summary_label: format!(
                    "{} · {}",
                    iso.chars().take(16).collect::<String>(),
                    summary
                ),
                full_changelog: String::new(),
                changelog: String::new(),
                diff: String::from("No single item diff is available for this snapshot."),
            };
            groups.push(WorklistHistoryGroup {
                id: format!("snapshot-{}", ts),
                title: if ids.is_empty() {
                    String::from("description changed")
                } else {
                    ids.join(", ")
                },
                ids,
                phases: vec![phase],
                latest_ts: ts,
                latest_iso: iso,
                phase_count: 0,
                subtitle: String::new(),
                kind: String::from("snapshot"),
                current_item: None,
                prose_phase_summary: String::new(),
            });
        }
    }

    for group in &mut groups {
        group.phase_count = group.phases.len();
        if group.phase_count > 1 {
            let first = group
                .phases
                .first()
                .map(|p| p.iso.clone())
                .unwrap_or_default();
            let last = group
                .phases
                .last()
                .map(|p| p.iso.clone())
                .unwrap_or_default();
            group.subtitle = format!(
                "{} phases · {}",
                group.phase_count,
                history_compact_range(&first, &last)
            );
        } else if let Some(phase) = group.phases.last() {
            group.subtitle = format!("{} · {}", history_compact_iso(&phase.iso), phase.summary);
        }
    }
    groups.sort_by(|a, b| b.latest_ts.cmp(&a.latest_ts));
    if limit > 0 && groups.len() > limit {
        groups.truncate(limit);
    }
    groups
}

#[cfg(test)]
mod worklist_history_tests {
    use super::{history_compact_range, resolve_worklist_doc_drafts, worklist_history_item_diff};
    use serde_json::json;

    #[test]
    fn item_diff_shows_status_transition() {
        let before = json!({"id": "x", "status": "proposed", "files": ["a"]});
        let after = json!({"id": "x", "status": "applied", "files": ["a"]});

        let diff = worklist_history_item_diff(Some(&before), Some(&after));

        assert!(diff.contains("- \"status\": \"proposed\""));
        assert!(diff.contains("+ \"status\": \"applied\""));
        assert!(!diff.contains("\"files\""));
    }

    #[test]
    fn item_diff_shows_removal() {
        let before = json!({"id": "x", "status": "applied"});

        let diff = worklist_history_item_diff(Some(&before), None);

        assert!(diff.contains("- {"));
        assert!(diff.contains("-   \"id\": \"x\""));
    }

    #[test]
    fn resolves_draft_prose_for_history_snapshots() {
        let dir =
            std::env::temp_dir().join(format!("bram-worklist-history-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp draft dir");
        std::fs::write(
            dir.join("draft-item.md"),
            "# Before\nold prose\n\n# After\nnew prose\n",
        )
        .expect("write draft");
        let doc = json!({
            "description": "test",
            "items": [{"id": "draft-item", "status": "proposed", "files": ["a.md"]}]
        });

        let resolved = resolve_worklist_doc_drafts(Some(&dir), &doc);
        let item = &resolved["items"][0];

        assert_eq!(
            item.get("before").and_then(|v| v.as_str()),
            Some("old prose")
        );
        assert_eq!(
            item.get("after").and_then(|v| v.as_str()),
            Some("new prose")
        );
        assert!(item.get("_draftMissing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_history_range_drops_redundant_year_month() {
        assert_eq!(
            history_compact_range("2026-05-29T10:00:00Z", "2026-05-29T10:45:00Z"),
            "29 10:00 -> 10:45"
        );
        assert_eq!(
            history_compact_range("2026-05-29T10:00:00Z", "2026-05-30T09:15:00Z"),
            "29 10:00 -> 30 09:15"
        );
    }
}

// Routing for the right-pane HTTP server. Returns (status, content-type, body).
fn route_request<R: tauri::Runtime>(
    app: &AppHandle<R>,
    path: &str,
    query: &str,
) -> (u16, &'static str, Vec<u8>) {
    if path == "__context/list" {
        let mut provider: Option<SessionProvider> = None;
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("provider=") {
                provider = SessionProvider::from_str(&percent_decode(enc));
                break;
            }
        }
        let body = serde_json::to_vec(&context_list(app, provider)).unwrap_or_default();
        return (200, "application/json; charset=utf-8", body);
    }

    if path == "__context/search" {
        let mut q = String::new();
        let mut provider: Option<SessionProvider> = None;
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("q=") {
                q = percent_decode(enc);
            } else if let Some(enc) = pair.strip_prefix("provider=") {
                provider = SessionProvider::from_str(&percent_decode(enc));
            }
        }
        let body = serde_json::to_vec(&context_search(app, provider, &q)).unwrap_or_default();
        return (200, "application/json; charset=utf-8", body);
    }

    if path == "__context/file" {
        let mut file_path = String::new();
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("path=") {
                file_path = percent_decode(enc);
                break;
            }
        }
        let p = std::path::Path::new(&file_path);
        let result = match std::fs::read_to_string(p) {
            Ok(content) => serde_json::json!({ "content": content }),
            Err(e) => {
                eprintln!("[http /__context/file path={}] {}", file_path, e);
                serde_json::json!({ "content": "", "error": e.to_string() })
            }
        };
        let body = serde_json::to_vec(&result).unwrap_or_default();
        return (200, "application/json; charset=utf-8", body);
    }

    if path == "__app-info" {
        let info = get_app_info();
        let body = serde_json::to_vec(&info).unwrap_or_default();
        return (200, "application/json; charset=utf-8", body);
    }

    if path == "__agent-status" {
        // Codex doesn't paint a rotating verb line and its silence
        // between turns is often shorter than the silence-clear gate
        // (10 s) used by pty_agent_turn_update — so without this
        // route-side check a stale "working" row would linger until
        // the next spinner glyph. Instead of clearing to idle, leave
        // Codex in "finished" until the next turn starts so the GUI
        // keeps an explicit completion cue.
        let codex_stale = if let Ok(cur) = agent_status_cell().lock() {
            cur.provider.as_deref() == Some("codex")
                && cur.state == "working"
                && unix_now_ms().saturating_sub(cur.updated_at_ms) > CODEX_AGENT_STATUS_STALE_MS
        } else {
            false
        };
        if codex_stale {
            agent_status_emit_finished(app, "codex", None, None);
        }
        let body = if let Ok(cur) = agent_status_cell().lock() {
            serde_json::to_vec(&*cur).unwrap_or_default()
        } else {
            b"{}".to_vec()
        };
        return (200, "application/json; charset=utf-8", body);
    }

    if path == "__event/latest" {
        let mut name = String::new();
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("name=") {
                name = percent_decode(enc);
                break;
            }
        }
        if name.is_empty() {
            return (
                400,
                "application/json; charset=utf-8",
                br#"{"ok":false,"error":"name query parameter required"}"#.to_vec(),
            );
        }
        let body = serde_json::to_vec(&latest_tauri_event_response(&name)).unwrap_or_default();
        return (200, "application/json; charset=utf-8", body);
    }

    if path == "__startup-ready" {
        let mut event = String::new();
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("event=") {
                event = percent_decode(enc);
                break;
            }
        }
        if event.is_empty() {
            return (
                400,
                "application/json; charset=utf-8",
                br#"{"ok":false,"error":"event query parameter required"}"#.to_vec(),
            );
        }
        let body = serde_json::to_vec(&replay_tauri_event_live(app, &event)).unwrap_or_default();
        return (200, "application/json; charset=utf-8", body);
    }

    if path == "__right-pane-info" {
        #[derive(serde::Serialize)]
        struct RightPaneInfo<'a> {
            url: &'a str,
            default_right_pane: &'a str,
            spawned: Option<&'a ServerConfig>,
        }
        let pane_state = app.state::<PaneUrlsState>();
        let urls = pane_state.0.lock().unwrap();
        let spawn_state = app.state::<SpawnedServerState>();
        let spawned_guard = spawn_state.0.lock().unwrap();
        let info = RightPaneInfo {
            url: &urls.right_pane,
            default_right_pane: &urls.default_right_pane,
            spawned: spawned_guard.as_ref().map(|s| &s.config),
        };
        let body = serde_json::to_vec(&info).unwrap_or_default();
        return (200, "application/json; charset=utf-8", body);
    }

    if path == "__restart-server" {
        use tauri::Emitter;

        let (cfg, pid) = {
            let spawn_state = app.state::<SpawnedServerState>();
            let mut guard = spawn_state.0.lock().unwrap();
            let Some(mut spawned) = guard.take() else {
                return (
                    400,
                    "text/plain; charset=utf-8",
                    b"no spawned project server".to_vec(),
                );
            };
            let cfg = spawned.config.clone();
            let Some(proj_root) = project_root(Some(app)) else {
                let body = serde_json::json!({
                    "ok": false,
                    "error": "no project root",
                });
                return (
                    500,
                    "application/json; charset=utf-8",
                    serde_json::to_vec(&body).unwrap_or_default(),
                );
            };

            let old_pid = spawned.child.id();
            let _ = spawned.child.kill();
            let _ = spawned.child.wait();
            eprintln!("[server] killed pid={} on manual restart", old_pid);

            match spawn_project_server(&cfg, &proj_root) {
                Ok(child) => {
                    let pid = child.id();
                    *guard = Some(SpawnedServer {
                        child,
                        config: cfg.clone(),
                    });
                    (cfg, pid)
                }
                Err(e) => {
                    eprintln!("[server] restart failed: {}", e);
                    let body = serde_json::json!({
                        "ok": false,
                        "error": e,
                    });
                    return (
                        500,
                        "application/json; charset=utf-8",
                        serde_json::to_vec(&body).unwrap_or_default(),
                    );
                }
            }
        };

        let port_up = wait_for_port(cfg.port, 5000);
        if !port_up {
            eprintln!(
                "[server] WARNING: restarted port {} did not come up within 5s; right-pane iframe will retry",
                cfg.port
            );
        } else {
            eprintln!("[server] restarted pid={}; port {} is up", pid, cfg.port);
        }
        trace_emit_signal(app, "right-pane-reload");
        let _ = app.emit("right-pane-reload", ());
        let body = serde_json::json!({
            "ok": true,
            "pid": pid,
            "port_up": port_up,
        });
        return (
            200,
            "application/json; charset=utf-8",
            serde_json::to_vec(&body).unwrap_or_default(),
        );
    }

    if path == "__error" {
        let mut reason = String::new();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("reason=") {
                reason = percent_decode(v);
                break;
            }
        }
        let escape = |s: &str| -> String {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let html = format!(
            "<!doctype html><meta charset=utf-8><title>Bram: project server unavailable</title>\
             <style>body{{font-family:system-ui,-apple-system,sans-serif;padding:32px;background:#1e1e1e;color:#e0e0e0;line-height:1.5}}\
             h1{{color:#ff7a7a;margin:0 0 16px;font-size:18px}}p{{margin:8px 0}}code{{background:#333;color:#e0e0e0;padding:2px 6px;border-radius:4px;font-family:Menlo,Monaco,monospace}}</style>\
             <h1>Bram: project server unavailable</h1>\
             <p>{}</p>",
            escape(&reason)
        );
        return (200, "text/html; charset=utf-8", html.into_bytes());
    }

    if path == "__commits" {
        return match git_log_recent(app, 100) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__commits] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__git/status" {
        return match git_status_summary(app) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__git/status] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__commits/search" {
        let mut q = String::new();
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("q=") {
                q = percent_decode(enc);
                break;
            }
        }
        return match git_log_search(app, &q) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__commits/search q={}] {}", q, e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__issues" {
        return match gh_issues_list(app) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__issues] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__issues/search" {
        let mut q = String::new();
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("q=") {
                q = percent_decode(enc);
                break;
            }
        }
        return match gh_issues_search(app, &q) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__issues/search q={}] {}", q, e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__issue" {
        let mut number: u64 = 0;
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("number=") {
                number = percent_decode(v).parse().unwrap_or(0);
                break;
            }
        }
        if number == 0 {
            return (400, "text/plain; charset=utf-8", b"missing number".to_vec());
        }
        return match gh_issue_view(app, number) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__issue number={}] {}", number, e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__issue/comment" {
        let mut number: u64 = 0;
        let mut body = String::new();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("number=") {
                number = percent_decode(v).parse().unwrap_or(0);
            } else if let Some(v) = pair.strip_prefix("body=") {
                body = percent_decode(v);
            }
        }
        if number == 0 {
            return (400, "text/plain; charset=utf-8", b"missing number".to_vec());
        }
        return match gh_issue_comment(app, number, &body) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__issue/comment number={}] {}", number, e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__issue/close" {
        let mut number: u64 = 0;
        let mut comment = String::new();
        let mut commit = String::new();
        let mut push_before_close = false;
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("number=") {
                number = percent_decode(v).parse().unwrap_or(0);
            } else if let Some(v) = pair.strip_prefix("comment=") {
                comment = percent_decode(v);
            } else if let Some(v) = pair.strip_prefix("commit=") {
                commit = percent_decode(v);
            } else if let Some(v) = pair.strip_prefix("push=") {
                let v = percent_decode(v);
                push_before_close = v == "1" || v.eq_ignore_ascii_case("true");
            }
        }
        if number == 0 {
            return (400, "text/plain; charset=utf-8", b"missing number".to_vec());
        }
        if !commit.trim().is_empty() {
            return gh_issue_close_with_commit(app, number, &commit, push_before_close);
        }
        return match gh_issue_close(app, number, &comment) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__issue/close number={}] {}", number, e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__repo/origin" {
        return match repo_origin_info(app) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__repo/origin] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__commit" {
        let mut sha = String::new();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("sha=") {
                sha = percent_decode(v);
                break;
            }
        }
        return match git_commit_detail(app, &sha) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__commit sha={}] {}", sha, e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__file" {
        let mut file_path = String::new();
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("path=") {
                file_path = percent_decode(enc);
                break;
            }
        }
        let p = std::path::Path::new(&file_path);
        return match std::fs::read(p) {
            Ok(bytes) => (200, mime_for(p), bytes),
            Err(e) => {
                eprintln!("[http /__file path={}] {}", file_path, e);
                (404, "text/plain; charset=utf-8", Vec::new())
            }
        };
    }

    // Architectural experiment: derive-at-the-boundary for the
    // "last assistant text" panel. Iframe binds to this route's
    // {text} field instead of calling lastAssistantText(lastJsonl) and
    // walking the buffer per fanout. Refetch is event-driven via
    // talk-session-changed.
    if path == "__last-assistant-text" {
        return match read_last_assistant_text(app, None) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__last-assistant-text] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if path == "__last-exchange" {
        return match read_last_exchange(app, None) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__last-exchange] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    // Companion to /__last-assistant-text: per-file edit aggregates for
    // the current turn. Same architecture (host parses 64 KB tail once
    // per request, iframe binds via DataSource), replaces the iframe's
    // currentTurnEdits(lastJsonl) helper which had started exceeding
    // XMLUI's 1000 ms sync-evaluation limit on busy turns.
    if path == "__current-turn-edits" {
        return match read_current_turn_edits(app, None) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__current-turn-edits] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    // Mirror of isWaitingForAssistant(jsonlText) iframe helper. Returns
    // {waiting: bool} — true when the most recent meaningful record is
    // a user message (tool_result-only records skipped). Replaces the
    // iframe-side suffix walk on every fanout / keystroke.
    if path == "__waiting-for-assistant" {
        return match read_waiting_for_assistant(app) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__waiting-for-assistant] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    // Size of the active session JSONL plus a green/amber/red state and
    // per-agent reset guidance. Polled by the Status tab so the user
    // gets a calm warning before the iframe-slowdown threshold is hit.
    if path == "__session-size" {
        let body = read_session_size(app).unwrap_or_else(|_| b"{}".to_vec());
        return (200, "application/json; charset=utf-8", body);
    }

    // Host-derived turn timeline. Mirrors the iframe sessionTurns(jsonlText)
    // helper that walked the full JSONL on every fanout. Returns the same
    // [{role, text, entries[], images[]}] shape Transcript renders against.
    if path == "__session-turns" {
        return match read_session_turns(app) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__session-turns] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    // Companion to /__session-turns: full input + result for a single
    // tool by id. Mirrors getToolDetail(jsonlText, toolId). Returns
    // {input, result} or null.
    if path == "__tool-detail" {
        let mut tool_id = String::new();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("id=") {
                tool_id = percent_decode(v);
                break;
            }
        }
        return match read_tool_detail(app, &tool_id) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__tool-detail] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    if let Some(rest) = path.strip_prefix("__sessions/") {
        let mut provider: Option<SessionProvider> = None;
        let mut session_id = String::new();
        let mut q = String::new();
        let mut scope = String::from("recent");
        let mut title = String::new();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("provider=") {
                provider = SessionProvider::from_str(&percent_decode(v));
            } else if let Some(v) = pair.strip_prefix("id=") {
                session_id = percent_decode(v);
            } else if let Some(v) = pair.strip_prefix("q=") {
                q = percent_decode(v);
            } else if let Some(v) = pair.strip_prefix("scope=") {
                scope = percent_decode(v);
            } else if let Some(v) = pair.strip_prefix("title=") {
                title = percent_decode(v);
            }
        }

        let (content_type, result): (&'static str, Result<Vec<u8>, String>) = if rest == "meta" {
            (
                "application/json; charset=utf-8",
                session_meta(app, provider)
                    .and_then(|meta| serde_json::to_vec(&meta).map_err(|e| e.to_string())),
            )
        } else if rest == "list" {
            (
                "application/json; charset=utf-8",
                list_sessions(app, provider)
                    .and_then(|entries| serde_json::to_vec(&entries).map_err(|e| e.to_string())),
            )
        } else if rest == "latest" {
            (
                "text/plain; charset=utf-8",
                read_latest_session(app, provider),
            )
        } else if rest == "latest-meta" {
            (
                "application/json; charset=utf-8",
                read_latest_session_meta(app, provider),
            )
        } else if rest == "latest-pending" {
            (
                "application/json; charset=utf-8",
                read_latest_session_pending(app, provider),
            )
        } else if rest == "latest-tail" {
            // Issue #100 / #71: diff-based response. Clients pass `?since=<N>`
            // and `?sid=<id>`; when sid matches the current latest session,
            // server returns bytes from offset `since` to EOF. Otherwise it
            // falls back to last-N-lines (or full file with `lines=all`).
            // Response is always a JSON envelope: { sid, offset, content, reset }
            // so the client can detect session rotation (sid change ⇒ reset)
            // and update its `since` cursor for the next poll.
            let mut lines_param: Option<String> = None;
            let mut since: u64 = 0;
            let mut expected_sid = String::new();
            for pair in query.split('&') {
                if let Some(v) = pair.strip_prefix("lines=") {
                    lines_param = Some(percent_decode(v));
                } else if let Some(v) = pair.strip_prefix("since=") {
                    since = percent_decode(v).parse().unwrap_or(0);
                } else if let Some(v) = pair.strip_prefix("sid=") {
                    expected_sid = percent_decode(v);
                }
            }
            // Resolve current latest session path; derive a stable sid from
            // the file stem so the diff response can carry it back to the client.
            let path_opt = latest_session_path(app, provider).unwrap_or(None);
            let (current_sid, file_size) = match &path_opt {
                Some(path) => {
                    let sid = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    (sid, size)
                }
                None => (String::new(), 0),
            };
            // since > 0 guards against the iframe reactivity race where
            // sessionSid is updated before sinceOffset — without this,
            // `since=0&sid=X` would read the whole file as an "incremental
            // delta from byte 0" (issue #100 smoke-test caught this).
            let incremental = !expected_sid.is_empty()
                && expected_sid == current_sid
                && since > 0
                && since <= file_size;
            let content_result: Result<Vec<u8>, String> = if incremental {
                match &path_opt {
                    Some(path) => {
                        use std::io::{Read, Seek, SeekFrom};
                        std::fs::File::open(path)
                            .map_err(|e| e.to_string())
                            .and_then(|mut f| {
                                f.seek(SeekFrom::Start(since)).map_err(|e| e.to_string())?;
                                let mut out = Vec::with_capacity((file_size - since) as usize);
                                f.read_to_end(&mut out).map_err(|e| e.to_string())?;
                                Ok(out)
                            })
                    }
                    None => Ok(Vec::new()),
                }
            } else {
                // Fresh fetch (no sid yet, or sid mismatch, or since past EOF).
                // Default-safe: lines absent or unparseable → last 200 records.
                // `lines=all` is the only path to the full file.
                match lines_param.as_deref() {
                    Some("all") => read_latest_session(app, provider),
                    None => read_latest_session_tail(app, provider, 200),
                    Some(s) => match s.parse::<usize>() {
                        Ok(n) => read_latest_session_tail(app, provider, n),
                        Err(_) => read_latest_session_tail(app, provider, 200),
                    },
                }
            };
            let result = content_result.and_then(|content| {
                let (content, truncated) = cap_latest_tail_payload(content);
                let appended = content.len();
                eprintln!(
                    "[latest-tail] mode={} sid={} since={} eof={} bytes={} truncated={}",
                    if incremental { "diff" } else { "fresh" },
                    current_sid,
                    since,
                    file_size,
                    appended,
                    truncated,
                );
                let envelope = serde_json::json!({
                    "sid": current_sid,
                    "offset": file_size,
                    "content": String::from_utf8_lossy(&content).into_owned(),
                    // reset=true ⇒ client REPLACES its lastJsonl buffer.
                    // reset=false ⇒ client APPENDS content. Authoritative
                    // signal so the client doesn't have to infer from
                    // sid equality (handles file-shrink case too).
                    "reset": !incremental || truncated,
                    "truncated": truncated,
                });
                serde_json::to_vec(&envelope).map_err(|e| e.to_string())
            });
            ("application/json; charset=utf-8", result)
        } else if rest == "content" {
            (
                "text/plain; charset=utf-8",
                read_session(app, &session_id, provider),
            )
        } else if rest == "search" {
            let limit = if scope == "all" { usize::MAX } else { 10 };
            (
                "application/json; charset=utf-8",
                search_sessions(app, &q, limit, provider)
                    .and_then(|entries| serde_json::to_vec(&entries).map_err(|e| e.to_string())),
            )
        } else if rest == "delete" {
            (
                "application/json; charset=utf-8",
                delete_session(app, &session_id, provider),
            )
        } else if rest == "rename" {
            (
                "application/json; charset=utf-8",
                rename_session(app, &session_id, provider, &title),
            )
        } else {
            (
                "text/plain; charset=utf-8",
                read_session(app, rest, provider),
            )
        };
        return match result {
            Ok(bytes) => (200, content_type, bytes),
            Err(e) => {
                eprintln!("[http /__sessions/{}] {}", rest, e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    // Enhance status / action: tells the agent tools banner whether the
    // current project has the conventions sidecar + CLAUDE.md import.
    if path == "__enhance/status" {
        return match enhance_status(app) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__enhance/status] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }
    if path == "__enhance/run" {
        let mut force = false;
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("force=") {
                force = matches!(percent_decode(enc).as_str(), "true" | "1");
                break;
            }
        }
        return match run_enhance(app, force) {
            Ok(bytes) => {
                eprintln!(
                    "[http /__enhance/run] force={} wrote sidecar + updated CLAUDE.md",
                    force
                );
                (200, "application/json; charset=utf-8", bytes)
            }
            Err(e) => {
                eprintln!("[http /__enhance/run] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }
    if path == "__enhance/codex-trust-ack" {
        return match write_codex_trust_ack() {
            Ok(()) => {
                emit_replayable_signal(app, "enhance-status-changed");
                (
                    200,
                    "application/json; charset=utf-8",
                    br#"{"ok":true}"#.to_vec(),
                )
            }
            Err(e) => {
                eprintln!("[http /__enhance/codex-trust-ack] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    // PTY rolling-buffer hex dump — for debugging menu detection.
    // Returns the last 2KB of PTY_TAIL as a hexdump (offsets, hex bytes,
    // ASCII gutter). Use to inspect what claude actually wrote when a
    // menu fails to render in the agent pane.
    if path == "__pty-tail" {
        let dump = match pty_tail_cell().lock() {
            Ok(tail) => {
                let n = tail.len();
                let start = n.saturating_sub(2048);
                hex_dump(&tail[start..])
            }
            Err(_) => String::from("(could not lock PTY_TAIL)\n"),
        };
        return (200, "text/plain; charset=utf-8", dump.into_bytes());
    }

    // strip_ansi'd view of PTY_TAIL — for inspecting exactly what the menu
    // detector matches against. Plain UTF-8 (lossy) so a human can read it.
    if path == "__pty-stripped" {
        let body = match pty_tail_cell().lock() {
            Ok(tail) => {
                let stripped = strip_ansi(&tail);
                String::from_utf8_lossy(&stripped).into_owned()
            }
            Err(_) => String::from("(could not lock PTY_TAIL)\n"),
        };
        return (200, "text/plain; charset=utf-8", body.into_bytes());
    }

    // PTY-tap menu detection — see pty_menu_update for rationale.
    // Returns {"menu": {"tool":..., "text":...}} when claude is currently
    // displaying its permission menu in the terminal, else {"menu": null}.
    if path == "__pty-menu" {
        let body = match pty_menu_cell().lock() {
            Ok(menu) => match &*menu {
                Some(m) => serde_json::json!({"menu": m}).to_string().into_bytes(),
                None => br#"{"menu":null}"#.to_vec(),
            },
            Err(_) => br#"{"menu":null}"#.to_vec(),
        };
        return (200, "application/json; charset=utf-8", body);
    }

    // /__worklist — same shape as /resources/worklist.json but with a
    // `diff` field injected on each `applied` item (the `git diff <file>`
    // output). The Workspace pane polls this so the TO COMMIT rows can
    // surface their pending diff inline.
    if path == "__worklist" {
        let mut doc = worklist_doc(app);
        if let Some(items) = doc.get_mut("items").and_then(|v| v.as_array_mut()) {
            for item in items {
                let status = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("proposed")
                    .to_string();
                if status != "applied" {
                    continue;
                }
                // Item scope: prefer `files: [...]` array, fall back to the
                // legacy single `file: <string>` for backward compat.
                let file_paths: Vec<String> =
                    if let Some(arr) = item.get("files").and_then(|v| v.as_array()) {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .filter(|s| !s.is_empty())
                            .collect()
                    } else if let Some(s) = item.get("file").and_then(|v| v.as_str()) {
                        if s.is_empty() {
                            Vec::new()
                        } else {
                            vec![s.to_string()]
                        }
                    } else {
                        Vec::new()
                    };
                if file_paths.is_empty() {
                    continue;
                }
                let mut combined = String::new();
                for fp in &file_paths {
                    let mut diff = git_run(app, &["diff", "--", fp]).unwrap_or_default();
                    if diff.is_empty() {
                        // git diff returns nothing for untracked files. Fall back
                        // to --no-index against /dev/null, which always produces
                        // an "add the whole file" diff. That command exits 1 when
                        // it finds differences, so git_run would treat it as an
                        // error and discard stdout — shell out directly here.
                        if let Some(root) = project_root(Some(app)) {
                            if let Ok(out) = std::process::Command::new("git")
                                .current_dir(&root)
                                .args(&["diff", "--no-index", "--", "/dev/null", fp])
                                .output()
                            {
                                diff = String::from_utf8_lossy(&out.stdout).into_owned();
                            }
                        }
                    }
                    if !combined.is_empty() && !diff.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&diff);
                }
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("diff".to_string(), serde_json::Value::String(combined));
                }
            }
        }
        let body = serde_json::to_vec(&doc).unwrap_or_default();
        return (200, "application/json; charset=utf-8", body);
    }

    if path == "__worklist/init" {
        return match init_worklist_file(app) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => (500, "text/plain; charset=utf-8", e.into_bytes()),
        };
    }

    if path == "__worklist-config" {
        let config = project_root(Some(app)).and_then(|root| load_project_config(&root));
        let enabled = project_config_batch_commit_actions(config);
        let body = serde_json::json!({ "batchCommitActions": enabled })
            .to_string()
            .into_bytes();
        return (200, "application/json; charset=utf-8", body);
    }

    // /__settings — user-facing slice of .bram.json. GET returns the merged
    // view (Settings tab + parent shell consume this); POST is handled
    // separately in handle_http's POST-only block. The Tauri
    // `settings-changed` event carries the same payload.
    if path == "__settings" {
        let config = project_root(Some(app)).and_then(|root| load_project_config(&root));
        let body = settings_view_from_config(config).to_string().into_bytes();
        return (200, "application/json; charset=utf-8", body);
    }

    // /__worklist/resolve[?ids=foo,bar] — verified-authorization endpoint
    // the agent reads instead of parsing the `approved:` / `drop:` turn
    // line. Returns the current `.worklist-authorization.json` body, with
    // optional id-filtering.
    //
    // Response kinds:
    //   - "approved" / "drop": active authorization, body carries items/ids.
    //     Approved records are consume-on-read here so a confused agent that
    //     reflexively calls the resolver on a non-authorization turn (iterate,
    //     talk) can't replay stale approval. Drop consumption stays in
    //     `maybe_enforce_worklist_policy` so authorized prunes aren't reverted.
    //   - "rejected_stale": on-disk worklist drifted between the user's click
    //     and the watcher reading it — agent should surface staleness.
    //   - "no_active_authorization": prior record has been consumed; the agent
    //     must NOT treat this as authorization. Returned for any consumed
    //     record regardless of original kind.
    if path == "__worklist/resolve" {
        let mut id_filter: Option<Vec<String>> = None;
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("ids=") {
                let decoded = percent_decode(enc);
                let parsed: Vec<String> = decoded
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !parsed.is_empty() {
                    id_filter = Some(parsed);
                }
                break;
            }
        }
        return handle_worklist_resolve(app, id_filter);
    }

    // /__inflight — host-managed inflight sentinel (#84). Returns the
    // contents of resources/.inflight-claim.json or `{}` if no claim is
    // active. The iframe refetches this on receipt of the
    // `inflight-claim-changed` Tauri event and derives spinner state
    // from the response (`ids` array, `kind`, `claimedAt`).
    if path == "__inflight" {
        let body = match inflight_claim_file(app) {
            Some(p) => std::fs::read_to_string(&p).unwrap_or_else(|_| "{}".to_string()),
            None => "{}".to_string(),
        };
        return (200, "application/json; charset=utf-8", body.into_bytes());
    }

    // /__coordination-status — compact host-side summary for the Status tab.
    // Keeps filesystem and trace mining in Rust so the XMLUI surface renders
    // one structured payload instead of fetching and parsing several files.
    if path == "__coordination-status" {
        return match coordination_status(app) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(e) => {
                eprintln!("[http /__coordination-status] {}", e);
                (500, "text/plain; charset=utf-8", e.into_bytes())
            }
        };
    }

    // /__pty-intent — diagnostic readout of the right-pane intent queue
    // (#86). Returns `{"queue": [<parsed-jsonl-line>, ...]}`. Empty
    // queue or missing file returns `{"queue": []}`. Read-only.
    if path == "__pty-intent" {
        let queue: Vec<serde_json::Value> = pty_intent_file(app)
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .map(|s| {
                s.lines()
                    .filter(|l| !l.is_empty())
                    .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                    .collect()
            })
            .unwrap_or_default();
        let body = serde_json::json!({ "queue": queue }).to_string();
        return (200, "application/json; charset=utf-8", body.into_bytes());
    }

    // /__git-diff?path=<file> — plain text `git diff -- <path>` output.
    if path == "__git-diff" {
        let mut file_path = String::new();
        for pair in query.split('&') {
            if let Some(enc) = pair.strip_prefix("path=") {
                file_path = percent_decode(enc);
                break;
            }
        }
        let diff = git_run(app, &["diff", "--", &file_path]).unwrap_or_default();
        return (200, "text/plain; charset=utf-8", diff.into_bytes());
    }

    // /__worklist-history/list — reverse-chronological list of snapshots
    // grouped by logical worklist item, with per-phase item-state diffs.
    if path == "__worklist-history/list" {
        let mut limit = WORKLIST_HISTORY_DEFAULT_LIMIT;
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("limit=") {
                limit = percent_decode(v)
                    .parse::<usize>()
                    .unwrap_or(WORKLIST_HISTORY_DEFAULT_LIMIT);
            }
        }
        let entries = recent_worklist_history_groups(app, limit);
        let body = serde_json::to_vec(&entries).unwrap_or_default();
        return (200, "application/json; charset=utf-8", body);
    }

    // /__worklist-history/changelog?ts=<unix_ms> — raw .md body
    if path == "__worklist-history/changelog" {
        let mut ts = String::new();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("ts=") {
                ts = percent_decode(v);
                break;
            }
        }
        let Some(dir) = worklist_history_dir(app) else {
            return (404, "text/plain; charset=utf-8", Vec::new());
        };
        let p = dir.join(format!("{}.md", ts));
        return match std::fs::read(&p) {
            Ok(bytes) => (200, "text/markdown; charset=utf-8", bytes),
            Err(_) => (404, "text/plain; charset=utf-8", Vec::new()),
        };
    }

    // /__worklist-history/search?q=<query> — filter the same group rows
    // /__worklist-history/list produces by substring-matching the
    // serialized JSON of each group (catches title, ids, subtitle, prose
    // summaries, before/after fields inside current_item). Case-insensitive.
    // Returns {results: [...]} (Commits/Issues shape, the new standard).
    if path == "__worklist-history/search" {
        let mut q = String::new();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("q=") {
                q = percent_decode(v);
                break;
            }
        }
        let trimmed = q.trim();
        if trimmed.len() < 2 {
            return (
                200,
                "application/json; charset=utf-8",
                br#"{"results":[]}"#.to_vec(),
            );
        }
        let needle = trimmed.to_lowercase();
        let groups = recent_worklist_history_groups(app, WORKLIST_HISTORY_DEFAULT_LIMIT);
        let mut results: Vec<serde_json::Value> = Vec::new();
        for g in &groups {
            let serialized = serde_json::to_string(g).unwrap_or_default();
            if !serialized.to_lowercase().contains(&needle) {
                continue;
            }
            // hitBody concatenates the human-readable text fields the modal
            // and card-snippet renderer can window into. Mirrors what the
            // serialized-JSON filter above matches against, but in a
            // reader-friendly format (no JSON syntax noise).
            let mut hit_body = String::new();
            hit_body.push_str(&g.title);
            hit_body.push('\n');
            if !g.subtitle.is_empty() {
                hit_body.push_str(&g.subtitle);
                hit_body.push('\n');
            }
            if !g.prose_phase_summary.is_empty() {
                hit_body.push_str(&g.prose_phase_summary);
                hit_body.push('\n');
            }
            if let Some(item) = g.current_item.as_ref() {
                if let Some(obj) = item.as_object() {
                    for key in ["before", "after", "feedback"] {
                        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
                            if !v.is_empty() {
                                hit_body.push_str(v);
                                hit_body.push('\n');
                            }
                        }
                    }
                }
            }
            // Center hitBody on the first match and cap at 4 KB. The
            // iframe modal uses a 500-char window and the card preview
            // uses 160; 4 KB gives both room without blowing up the
            // payload size (a "pe" query against 120 groups was 949 KB
            // before this cap; per-row cap drops it to ~120 * 4 KB ≈
            // 480 KB worst case, and typical groups are smaller than
            // the cap so the real reduction is larger).
            const HIT_BODY_CAP: usize = 4096;
            let lower_body = hit_body.to_lowercase();
            let centered = if let Some(pos) = lower_body.find(&needle) {
                let half = HIT_BODY_CAP / 2;
                let start = pos.saturating_sub(half);
                let end = (start + HIT_BODY_CAP).min(hit_body.len());
                let start = end.saturating_sub(HIT_BODY_CAP);
                hit_body.get(start..end).unwrap_or("").to_string()
            } else {
                hit_body.chars().take(HIT_BODY_CAP).collect()
            };
            let mut row = serde_json::to_value(g).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = row.as_object_mut() {
                obj.insert("hitBody".to_string(), serde_json::Value::String(centered));
            }
            results.push(row);
        }
        let body = serde_json::json!({ "results": results })
            .to_string()
            .into_bytes();
        return (200, "application/json; charset=utf-8", body);
    }

    // /__feedback-history/search?q=<query> — substring-match the .md body
    // of each entry, return {ts, itemId, fileName, snippet}. Snippet is a
    // ~200-char window centered on the first match. Case-insensitive.
    if path == "__feedback-history/search" {
        let mut q = String::new();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("q=") {
                q = percent_decode(v);
                break;
            }
        }
        let trimmed = q.trim();
        if trimmed.len() < 2 {
            return (
                200,
                "application/json; charset=utf-8",
                br#"{"results":[]}"#.to_vec(),
            );
        }
        let needle = trimmed.to_lowercase();
        let entries = recent_feedback_history(app, WORKLIST_HISTORY_DEFAULT_LIMIT);
        let mut results: Vec<serde_json::Value> = Vec::new();
        if let Some(dir) = feedback_history_dir(app) {
            for entry in entries {
                let Some(file_name) = entry.get("fileName").and_then(|v| v.as_str()) else {
                    continue;
                };
                let p = dir.join(file_name);
                let body = std::fs::read_to_string(&p).unwrap_or_default();
                let lower = body.to_lowercase();
                let Some(pos) = lower.find(&needle) else {
                    continue;
                };
                let start = pos.saturating_sub(80);
                let end = (pos + needle.len() + 120).min(body.len());
                let snippet = body.get(start..end).unwrap_or("").replace('\n', " ");
                let mut row = entry.clone();
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("snippet".to_string(), serde_json::Value::String(snippet));
                    obj.insert("body".to_string(), serde_json::Value::String(body.clone()));
                }
                results.push(row);
            }
        }
        let body = serde_json::json!({ "results": results })
            .to_string()
            .into_bytes();
        return (200, "application/json; charset=utf-8", body);
    }

    // /__feedback-history/list[?limit=N] — reverse-chronological list of
    // feedback drafts promoted from resources/feedback-drafts/ to
    // resources/feedback-history/ by promote_feedback_drafts_for_items.
    // Each entry: { ts, itemId, fileName }. The Feedback tab consumes this.
    if path == "__feedback-history/list" {
        let mut limit = WORKLIST_HISTORY_DEFAULT_LIMIT;
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("limit=") {
                limit = percent_decode(v)
                    .parse::<usize>()
                    .unwrap_or(WORKLIST_HISTORY_DEFAULT_LIMIT);
            }
        }
        let entries = recent_feedback_history(app, limit);
        let body = serde_json::to_vec(&entries).unwrap_or_default();
        return (200, "application/json; charset=utf-8", body);
    }

    // /__feedback-history/content?ts=<unix_ms>&itemId=<id> — raw .md body
    // for one entry. Reconstructs the filename rather than trusting a
    // client-supplied path (no traversal).
    if path == "__feedback-history/content" {
        let mut ts = String::new();
        let mut item_id = String::new();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("ts=") {
                ts = percent_decode(v);
            } else if let Some(v) = pair.strip_prefix("itemId=") {
                item_id = percent_decode(v);
            }
        }
        if ts.is_empty()
            || item_id.is_empty()
            || ts.chars().any(|c| !c.is_ascii_digit())
            || item_id.contains('/')
            || item_id.contains('\\')
        {
            return (400, "text/plain; charset=utf-8", b"bad params".to_vec());
        }
        let Some(dir) = feedback_history_dir(app) else {
            return (404, "text/plain; charset=utf-8", Vec::new());
        };
        let p = dir.join(format!("{}-{}.md", ts, item_id));
        return match std::fs::read(&p) {
            Ok(bytes) => (200, "text/markdown; charset=utf-8", bytes),
            Err(_) => (404, "text/plain; charset=utf-8", Vec::new()),
        };
    }

    // /__worklist-history/snapshot?ts=<unix_ms> — raw .json snapshot
    if path == "__worklist-history/snapshot" {
        let mut ts = String::new();
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("ts=") {
                ts = percent_decode(v);
                break;
            }
        }
        let Some(dir) = worklist_history_dir(app) else {
            return (404, "text/plain; charset=utf-8", Vec::new());
        };
        let p = dir.join(format!("{}.json", ts));
        return match std::fs::read(&p) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(_) => (404, "text/plain; charset=utf-8", Vec::new()),
        };
    }

    // worklist.json is the worklist convention. Treat it as always-present
    // (empty when the project hasn't opted in) so the Workspace tool can
    // poll without flooding devtools with 404s in guest projects.
    if path == "resources/worklist.json" {
        let proj = project_root(Some(app)).unwrap_or_else(|| PathBuf::from("."));
        return match std::fs::read(proj.join(path)) {
            Ok(bytes) => (200, "application/json; charset=utf-8", bytes),
            Err(_) => (
                200,
                "application/json; charset=utf-8",
                empty_worklist_json().as_bytes().to_vec(),
            ),
        };
    }

    // System namespaces served from the binary's bundled app/ dir
    // (on-disk if present, embedded otherwise).
    let app_rel: Option<String> = if let Some(rest) = path.strip_prefix("__shell/") {
        Some(format!("__shell/{}", rest))
    } else if let Some(rest) = path.strip_prefix("__vendor/") {
        Some(format!("vendor/{}", rest))
    } else if let Some(rest) = path.strip_prefix("__tools/") {
        Some(format!("tools/{}", rest))
    } else {
        None
    };
    if let Some(rel) = app_rel {
        return match serve_app_file(Some(app), &rel) {
            Some((bytes, mime)) => (200, mime, bytes),
            None => (404, "text/plain; charset=utf-8", Vec::new()),
        };
    }

    // Project-relative paths everywhere else.
    let proj = project_root(Some(app)).unwrap_or_else(|| PathBuf::from("."));
    let full = proj.join(path);
    match std::fs::read(&full) {
        Ok(bytes) => (200, mime_for(&full), bytes),
        Err(_) => (404, "text/plain; charset=utf-8", Vec::new()),
    }
}

// Shared resolver for the worklist authorization record. Backs both the HTTP
// route GET /__worklist/resolve and the Codex filesystem intent drain (#130),
// so both transports apply the identical consume-on-read + inflight-sentinel
// side effects. Response kinds are documented at the route's call site.
fn handle_worklist_resolve<R: tauri::Runtime>(
    app: &AppHandle<R>,
    id_filter: Option<Vec<String>>,
) -> (u16, &'static str, Vec<u8>) {
    let Some(auth_path) = worklist_auth_file(app) else {
        return (
            404,
            "text/plain; charset=utf-8",
            b"no project root".to_vec(),
        );
    };
    let Ok(raw) = std::fs::read_to_string(&auth_path) else {
        return (
            404,
            "text/plain; charset=utf-8",
            b"no authorization record".to_vec(),
        );
    };
    let mut record_value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            return (
                500,
                "text/plain; charset=utf-8",
                b"malformed authorization record".to_vec(),
            );
        }
    };
    let consumed_at = record_value.get("consumedAtMs").and_then(|v| v.as_i64());
    if let Some(ts) = consumed_at {
        let body = serde_json::json!({
            "kind": "no_active_authorization",
            "consumedAtMs": ts,
        })
        .to_string()
        .into_bytes();
        return (200, "application/json; charset=utf-8", body);
    }
    if let Some(filter) = id_filter {
        if let Some(items) = record_value.get_mut("items").and_then(|v| v.as_array_mut()) {
            items.retain(|it| {
                it.get("id")
                    .and_then(|v| v.as_str())
                    .map_or(false, |id| filter.iter().any(|f| f == id))
            });
        }
        if let Some(ids_v) = record_value.get_mut("ids").and_then(|v| v.as_array_mut()) {
            ids_v.retain(|v| {
                v.as_str()
                    .map_or(false, |id| filter.iter().any(|f| f == id))
            });
        }
    }
    resolve_worklist_record_items(app, &mut record_value);
    let kind = record_value
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Write the inflight sentinel for approved/drop (#84). Pulled
    // from the (possibly filter-narrowed) items array so the
    // sentinel's ids match what the agent has just been authorized
    // to act on. Cleared by /__worklist/mutate when the agent
    // completes the state transition.
    if kind == "approved" || kind == "drop" {
        let sentinel_ids: Vec<String> = record_value
            .get("items")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|it| it.get("id").and_then(|v| v.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if !sentinel_ids.is_empty() {
            write_inflight_claim_sentinel(app, &sentinel_ids, &kind);
        }
    }
    let body = record_value.to_string().into_bytes();
    if kind == "approved" {
        consume_worklist_authorization(app);
    }
    (200, "application/json; charset=utf-8", body)
}

// Codex filesystem lifecycle channel (#130). Codex's sandboxed curl can't
// reach Bram's loopback HTTP server, so instead of curling the lifecycle
// routes it writes resources/.worklist-intent.json; this drain (invoked from
// the filesystem watcher) dispatches the request through the SAME handlers the
// HTTP routes use and writes the reply to resources/.worklist-result.json,
// then deletes the intent file. The intent file is only a request envelope —
// the authority (hash-verified .worklist-authorization.json, the mutate auth
// check) is unchanged, so this transport grants Codex no power it lacked.
//
// Intent shape:  {"nonce": "...", "route": "<r>", "body": { ... }}
//   routes: worklist-resolve | worklist-mutate | iterate-begin |
//           iterate-end | worklist-end | issue-close
// Result shape:  {"nonce": "...", "ok": <bool>, "status": <u16>,
//                 "result"|"error": <json>, "completedAtMs": <ms>}
fn drain_worklist_intent<R: tauri::Runtime>(app: &AppHandle<R>) {
    let Some(intent_path) = worklist_intent_file(app) else {
        return;
    };
    // File may already be gone (a prior event in the same notify burst drained
    // it) — that's the natural dedup, not an error.
    let Ok(raw) = std::fs::read_to_string(&intent_path) else {
        return;
    };
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    let nonce = parsed
        .get("nonce")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let route = parsed.get("route").and_then(|v| v.as_str()).unwrap_or("");
    let body_val = parsed
        .get("body")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let body_bytes = serde_json::to_vec(&body_val).unwrap_or_default();

    let (status, resp_bytes): (u16, Vec<u8>) = if parsed.is_null() {
        (400, br#"{"error":"malformed intent JSON"}"#.to_vec())
    } else {
        match route {
            "worklist-resolve" => {
                let id_filter = body_val
                    .get("ids")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect::<Vec<String>>()
                    })
                    .filter(|v| !v.is_empty());
                let (s, _m, b) = handle_worklist_resolve(app, id_filter);
                (s, b)
            }
            "worklist-mutate" => {
                let (s, _m, b) = handle_worklist_mutate(app, &body_bytes);
                (s, b)
            }
            "iterate-begin" => {
                let (s, _m, b) = handle_iterate_begin(app, &body_bytes);
                (s, b)
            }
            "iterate-end" | "worklist-end" => {
                let (s, _m, b) = handle_iterate_end(app, &body_bytes);
                (s, b)
            }
            "issue-close" => {
                let (s, _m, b) = handle_issue_close_json(app, &body_bytes);
                (s, b)
            }
            other => (
                400,
                format!("{{\"error\":\"unknown route: {}\"}}", other).into_bytes(),
            ),
        }
    };

    let ok = (200..300).contains(&status);
    let payload: serde_json::Value = serde_json::from_slice(&resp_bytes)
        .unwrap_or_else(|_| serde_json::json!({ "raw": String::from_utf8_lossy(&resp_bytes) }));
    let mut envelope = serde_json::json!({
        "nonce": nonce,
        "ok": ok,
        "status": status,
        "completedAtMs": unix_now_ms(),
    });
    if let Some(obj) = envelope.as_object_mut() {
        obj.insert(if ok { "result" } else { "error" }.to_string(), payload);
    }
    if let Some(result_path) = worklist_result_file(app) {
        if let Some(parent) = result_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = result_path.with_extension("json.tmp");
        if std::fs::write(&tmp, format!("{}\n", envelope)).is_ok() {
            let _ = std::fs::rename(&tmp, &result_path);
        }
    }
    let _ = std::fs::remove_file(&intent_path);
    if bram_trace_enabled() {
        append_bram_trace_line(
            app,
            "worklist-intent",
            &format!(
                "route={} nonce={} ok={} status={}",
                route, nonce, ok, status
            ),
        );
    }
}

// POST /__worklist/mutate — server-side mechanical mutations (prune,
// advance status) symmetric to /__worklist/resolve. This is the
// canonical state-machine path for approval-driven worklist transitions;
// direct edits to resources/worklist.json remain for authoring/refining
// items, not for mechanical prune/advance. The agent issues a one-line
// curl instead of an Edit on resources/worklist.json, so the chat
// doesn't render a diff. Authorization is checked against
// resources/.worklist-authorization.json before the write: prune
// requires `kind: "drop"`, advance requires `kind: "approved"`, and
// every requested id must appear in the auth record's ids.
// POST /__iterate/begin — the agent calls this at the start of any
// iterate cycle. Writes the inflight sentinel with kind="iterate" and
// the ids from the iterate payload. Refs #84.
fn handle_iterate_begin<R: tauri::Runtime>(
    app: &AppHandle<R>,
    body: &[u8],
) -> (u16, &'static str, Vec<u8>) {
    let req_json: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                400,
                "application/json; charset=utf-8",
                format!("{{\"error\":\"invalid JSON: {}\"}}", e).into_bytes(),
            );
        }
    };
    let ids: Vec<String> = req_json
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return (
            400,
            "application/json; charset=utf-8",
            br#"{"error":"ids[] required"}"#.to_vec(),
        );
    }
    write_inflight_claim_sentinel(app, &ids, "iterate");
    (
        200,
        "application/json; charset=utf-8",
        br#"{"ok":true}"#.to_vec(),
    )
}

// POST /__iterate/end — the agent calls this at the end of any
// iterate cycle. Clears the sentinel if it fully covers the supplied
// ids (same coverage rule as /__worklist/mutate). Refs #84.
fn handle_iterate_end<R: tauri::Runtime>(
    app: &AppHandle<R>,
    body: &[u8],
) -> (u16, &'static str, Vec<u8>) {
    let req_json: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                400,
                "application/json; charset=utf-8",
                format!("{{\"error\":\"invalid JSON: {}\"}}", e).into_bytes(),
            );
        }
    };
    let ids: Vec<String> = req_json
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return (
            400,
            "application/json; charset=utf-8",
            br#"{"error":"ids[] required"}"#.to_vec(),
        );
    }
    record_turn_completion_monitor(
        "iterate-end",
        "agent",
        "explicit-end",
        format!("Iterate end covering {}", ids.join(", ")),
        unix_now_ms(),
        inflight_claim_ids_and_claimed_at(app)
            .map(|(claim_ids, claimed_at)| !claim_ids.is_empty() && unix_now_ms() >= claimed_at)
            .unwrap_or(false),
    );
    clear_inflight_claim_sentinel(app, &ids);
    (
        200,
        "application/json; charset=utf-8",
        br#"{"ok":true}"#.to_vec(),
    )
}

fn worklist_mutate_required_kind(op: &str) -> Result<&'static str, String> {
    match op {
        "prune" => Ok("drop"),
        "advance" => Ok("approved"),
        _ => Err(format!("unknown op: {}", op)),
    }
}

fn worklist_json_ids(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn validate_worklist_mutate_authorization(
    op: &str,
    ids: &[String],
    auth: &serde_json::Value,
) -> Result<String, String> {
    let required_kind = worklist_mutate_required_kind(op)?;
    let auth_kind = auth.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let kind_ok = auth_kind == required_kind || (op == "prune" && auth_kind == "approved");
    if !kind_ok {
        return Err(format!(
            "auth kind mismatch: expected {}{}, got {}",
            required_kind,
            if op == "prune" { " or approved" } else { "" },
            auth_kind
        ));
    }

    let auth_ids = worklist_json_ids(auth, "ids");
    for id in ids {
        if !auth_ids.iter().any(|aid| aid == id) {
            return Err(format!("id not in auth: {}", id));
        }
    }

    Ok(auth_kind.to_string())
}

fn worklist_item_status_for_id(items: &[serde_json::Value], id: &str) -> String {
    items
        .iter()
        .find(|it| it.get("id").and_then(|v| v.as_str()) == Some(id))
        .and_then(|it| it.get("status").and_then(|v| v.as_str()))
        .unwrap_or("proposed")
        .to_string()
}

fn validate_post_commit_prune_status(
    op: &str,
    auth_kind: &str,
    ids: &[String],
    items: &[serde_json::Value],
) -> Result<(), String> {
    if op != "prune" || auth_kind != "approved" {
        return Ok(());
    }
    for id in ids {
        let status = worklist_item_status_for_id(items, id);
        if status != "applied" {
            return Err(format!(
                "post-commit prune requires applied status: {} is {}",
                id, status
            ));
        }
    }
    Ok(())
}

fn validate_worklist_advance_status(op: &str, new_status: &str) -> Result<(), String> {
    if op != "advance" {
        return Ok(());
    }
    if new_status == "applied" {
        return Ok(());
    }
    Err(format!(
        "unsupported advance status: {}; use op:\"prune\" after a successful commit",
        new_status
    ))
}

fn apply_worklist_mutation(
    items: &mut Vec<serde_json::Value>,
    op: &str,
    ids: &[String],
    new_status: &str,
) -> Vec<String> {
    let mut affected: Vec<String> = Vec::new();
    if op == "prune" {
        items.retain(|item| {
            let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if ids.iter().any(|id| id == item_id) {
                affected.push(item_id.to_string());
                false
            } else {
                true
            }
        });
    } else {
        for item in items.iter_mut() {
            let item_id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if ids.iter().any(|id| id == &item_id) {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert(
                        "status".to_string(),
                        serde_json::Value::String(new_status.to_string()),
                    );
                    affected.push(item_id);
                }
            }
        }
    }
    affected
}

fn handle_worklist_mutate<R: tauri::Runtime>(
    app: &AppHandle<R>,
    body: &[u8],
) -> (u16, &'static str, Vec<u8>) {
    let req_json: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                400,
                "application/json; charset=utf-8",
                format!("{{\"error\":\"invalid JSON: {}\"}}", e).into_bytes(),
            );
        }
    };
    let op = req_json.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let ids = worklist_json_ids(&req_json, "ids");
    if ids.is_empty() {
        return (
            400,
            "application/json; charset=utf-8",
            br#"{"error":"ids[] required"}"#.to_vec(),
        );
    }
    if let Err(e) = worklist_mutate_required_kind(op) {
        return (
            400,
            "application/json; charset=utf-8",
            serde_json::json!({ "error": e }).to_string().into_bytes(),
        );
    }

    // Auth check. Deliberately ignores consumedAtMs: same-turn
    // resolve -> edit files -> mutate is valid, and resolve's
    // consume-on-read is only meant to block future resolver reads
    // from replaying stale approval.
    let auth_path = match worklist_auth_file(app) {
        Some(p) => p,
        None => {
            return (
                500,
                "application/json; charset=utf-8",
                br#"{"error":"no project root"}"#.to_vec(),
            );
        }
    };
    let auth: serde_json::Value = std::fs::read_to_string(&auth_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let auth_kind = match validate_worklist_mutate_authorization(op, &ids, &auth) {
        Ok(kind) => kind,
        Err(e) => {
            return (
                400,
                "application/json; charset=utf-8",
                serde_json::json!({ "error": e }).to_string().into_bytes(),
            );
        }
    };

    // Apply the op to worklist.json.
    let wl_path = match worklist_file(app) {
        Some(p) => p,
        None => {
            return (
                500,
                "application/json; charset=utf-8",
                br#"{"error":"no project root"}"#.to_vec(),
            );
        }
    };
    let mut wl: serde_json::Value = std::fs::read_to_string(&wl_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({"items":[]}));
    let items = match wl.get_mut("items").and_then(|v| v.as_array_mut()) {
        Some(arr) => arr,
        None => {
            return (
                500,
                "application/json; charset=utf-8",
                br#"{"error":"worklist missing items[]"}"#.to_vec(),
            );
        }
    };

    // Post-commit prune safeguard: pruning with kind=approved is only
    // allowed when every requested id is already status=applied —
    // blocks an agent from pruning an as-yet-unapplied approved item,
    // which would lose the work.
    if let Err(e) = validate_post_commit_prune_status(op, &auth_kind, &ids, items) {
        return (
            400,
            "application/json; charset=utf-8",
            serde_json::json!({ "error": e }).to_string().into_bytes(),
        );
    }

    let new_status = req_json
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("applied");
    if let Err(e) = validate_worklist_advance_status(op, new_status) {
        return (
            400,
            "application/json; charset=utf-8",
            serde_json::json!({ "error": e }).to_string().into_bytes(),
        );
    }
    let affected = apply_worklist_mutation(items, op, &ids, new_status);

    let new_text = serde_json::to_string_pretty(&wl).unwrap_or_default();
    let on_disk = format!("{}\n", new_text);
    // Claim this write BEFORE it lands so the worklist watcher recognizes
    // it as self-originated: handle_worklist_change short-circuits when
    // prior_str == current_str, so it never reaches maybe_enforce_worklist_policy.
    // Setting the cache after the write would leave a window where the watcher
    // fires first, reads the pruned content as an unauthorized removal (the drop
    // auth was just consumed, so read_active_worklist_authorization returns None),
    // and reverts it — the false-success-drop race. Matches the claim that
    // init_worklist_file / run_enhance already perform after their writes.
    let prior_cache = match last_worklist_cell().lock() {
        Ok(mut guard) => guard.replace(on_disk.clone()),
        Err(_) => None,
    };
    if let Err(e) = std::fs::write(&wl_path, &on_disk) {
        // The failed write never reached disk; restore the prior snapshot so
        // the watcher doesn't diff against content that was never written.
        if let Ok(mut guard) = last_worklist_cell().lock() {
            *guard = prior_cache;
        }
        return (
            500,
            "application/json; charset=utf-8",
            format!("{{\"error\":\"write failed: {}\"}}", e).into_bytes(),
        );
    }

    // Successful mutate is the mechanical completion point for approved/drop
    // worklist cycles. Clear any matching inflight sentinel immediately so the
    // Workspace spinner does not wait for the later silence-detected fallback.
    //
    // A drop prune can be a valid no-op if the item was already removed before
    // the agent retries /__worklist/mutate. Treat the requested ids as complete
    // in that case so the authorization and spinner do not linger forever.
    let completion_ids = if affected.is_empty() && op == "prune" {
        &ids
    } else {
        &affected
    };
    if !completion_ids.is_empty() {
        record_turn_completion_monitor(
            "mutate",
            "agent",
            op,
            format!("{} covering {}", op, completion_ids.join(", ")),
            unix_now_ms(),
            inflight_claim_ids_and_claimed_at(app)
                .map(|(claim_ids, claimed_at)| !claim_ids.is_empty() && unix_now_ms() >= claimed_at)
                .unwrap_or(false),
        );
        let cleared = clear_inflight_claim_sentinel(app, completion_ids);
        if !cleared {
            // No sentinel existed to clear — the agent reached mutate without a
            // prior /__worklist/resolve (the Codex filesystem path skips resolve;
            // refs #133). Emit the reconcile signal anyway so the Worklist iframe
            // refetches /__inflight and clears its click-time optimistic
            // `submitting`, instead of orphaning the spinner.
            if bram_trace_enabled() {
                append_bram_trace_line(
                    app,
                    "inflight-sentinel",
                    &format!(
                        "op=reconcile-no-claim ids={}",
                        serde_json::to_string(completion_ids).unwrap_or_else(|_| "[]".to_string())
                    ),
                );
            }
            emit_replayable_signal(app, "inflight-claim-changed");
        }
    }
    if op == "prune" && auth_kind == "drop" {
        consume_worklist_authorization(app);
    }
    promote_feedback_drafts_for_items(app, completion_ids, op);

    let result_key = if op == "prune" { "pruned" } else { "advanced" };
    let response = format!(
        "{{\"ok\":true,\"{}\":{}}}",
        result_key,
        serde_json::to_string(&affected).unwrap_or_else(|_| "[]".to_string())
    );
    (
        200,
        "application/json; charset=utf-8",
        response.into_bytes(),
    )
}

#[cfg(test)]
mod worklist_authorization_tests {
    use super::{
        apply_worklist_mutation, build_worklist_authorization_record, canonical_item_hash,
        classify_worklist_removals, draft_markdown_path, feedback_draft_path,
        inflight_claim_fully_covered, parse_worklist_authorization_message, resource_relative_path,
        validate_post_commit_prune_status, validate_worklist_advance_status,
        validate_worklist_mutate_authorization, worklist_draft_path,
    };
    use serde_json::json;
    use std::path::Path;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn approval_record_verifies_hash_and_embeds_feedback() {
        let dir = std::env::temp_dir().join(format!("bram-approval-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp draft dir");
        std::fs::write(
            dir.join("doc-update.md"),
            "# Before\n\nold\n\n# After\n\nnew\n",
        )
        .expect("write draft");

        // Draft-only: item carries metadata only; prose lives in the draft.
        let item = json!({
            "id": "doc-update",
            "status": "proposed",
            "file": "docs/a.md",
        });
        // Hash covers the resolved item (metadata + draft prose) — same shape
        // the agent computes after reading /__worklist.
        let resolved = super::resolve_worklist_item_draft(Some(&dir), &item);
        let hash = canonical_item_hash(&resolved);
        let msg = format!(
            r#"approved: {{"items":[{{"id":"doc-update","hash":"{}","feedback":"tighten scope"}}]}}"#,
            hash
        );

        let parsed = parse_worklist_authorization_message(&msg).expect("approval parses");
        let record =
            build_worklist_authorization_record(parsed, &[item], Some(&dir), 123, "test-source");

        assert_eq!(record.kind, "approved");
        assert_eq!(record.ids, ids(&["doc-update"]));
        assert!(record.mismatched_ids.is_empty());
        assert_eq!(record.items.len(), 1);
        assert_eq!(
            record.items[0].get("feedback").and_then(|v| v.as_str()),
            Some("tighten scope")
        );
        assert_eq!(record.issued_at_ms, 123);
        assert_eq!(record.source, "test-source");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approval_record_rejects_stale_hashes() {
        let item = json!({
            "id": "doc-update",
            "status": "proposed",
            "file": "docs/a.md",
            "before": "current",
            "after": "new"
        });
        let parsed = parse_worklist_authorization_message(
            r#"approved: {"items":[{"id":"doc-update","hash":"0000000000000000"}]}"#,
        )
        .expect("approval parses");

        let record = build_worklist_authorization_record(parsed, &[item], None, 123, "test");

        assert_eq!(record.kind, "rejected_stale");
        assert_eq!(record.ids, ids(&["doc-update"]));
        assert_eq!(record.mismatched_ids, ids(&["doc-update"]));
        assert!(record.items.is_empty());
    }

    #[test]
    fn legacy_drop_payload_has_ids_but_no_verified_items() {
        let item = json!({
            "id": "old-drop",
            "status": "proposed",
            "file": "docs/a.md",
            "before": "old",
            "after": "new"
        });
        let parsed = parse_worklist_authorization_message(r#"drop: {"ids":["old-drop"]}"#)
            .expect("drop parses");

        let record = build_worklist_authorization_record(parsed, &[item], None, 123, "test");

        assert_eq!(record.kind, "drop");
        assert_eq!(record.ids, ids(&["old-drop"]));
        assert!(record.mismatched_ids.is_empty());
        assert!(record.items.is_empty());
    }

    #[test]
    fn mutate_authorization_rejects_wrong_kind_and_missing_ids() {
        let auth = json!({"kind": "drop", "ids": ["a"]});

        let wrong_kind = validate_worklist_mutate_authorization("advance", &ids(&["a"]), &auth)
            .expect_err("advance requires approved auth");
        assert!(wrong_kind.contains("auth kind mismatch"));

        let missing_id = validate_worklist_mutate_authorization("prune", &ids(&["b"]), &auth)
            .expect_err("id must be covered by auth");
        assert_eq!(missing_id, "id not in auth: b");
    }

    #[test]
    fn approved_prune_requires_applied_status() {
        let proposed_items = vec![json!({"id": "a", "status": "proposed"})];
        let applied_items = vec![json!({"id": "a", "status": "applied"})];

        let err =
            validate_post_commit_prune_status("prune", "approved", &ids(&["a"]), &proposed_items)
                .expect_err("approved prune is post-commit only");
        assert_eq!(
            err,
            "post-commit prune requires applied status: a is proposed"
        );

        validate_post_commit_prune_status("prune", "approved", &ids(&["a"]), &applied_items)
            .expect("applied item can be pruned after commit");
    }

    #[test]
    fn advance_rejects_non_applied_status() {
        validate_worklist_advance_status("advance", "applied")
            .expect("applied is the only active advance target");

        let err = validate_worklist_advance_status("advance", "committed")
            .expect_err("committed items should be pruned, not advanced");
        assert_eq!(
            err,
            "unsupported advance status: committed; use op:\"prune\" after a successful commit"
        );

        let err = validate_worklist_advance_status("advance", "proposed")
            .expect_err("advancing to proposed is undefined");
        assert_eq!(
            err,
            "unsupported advance status: proposed; use op:\"prune\" after a successful commit"
        );

        validate_worklist_advance_status("prune", "committed")
            .expect("prune does not use target status");
    }

    #[test]
    fn apply_worklist_mutation_advances_and_prunes_only_requested_items() {
        let mut items = vec![
            json!({"id": "a", "status": "proposed"}),
            json!({"id": "b", "status": "proposed"}),
        ];

        let advanced = apply_worklist_mutation(&mut items, "advance", &ids(&["a"]), "applied");
        assert_eq!(advanced, ids(&["a"]));
        assert_eq!(
            items[0].get("status").and_then(|v| v.as_str()),
            Some("applied")
        );
        assert_eq!(
            items[1].get("status").and_then(|v| v.as_str()),
            Some("proposed")
        );

        let pruned = apply_worklist_mutation(&mut items, "prune", &ids(&["a"]), "applied");
        assert_eq!(pruned, ids(&["a"]));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].get("id").and_then(|v| v.as_str()), Some("b"));
    }

    #[test]
    fn classify_worklist_removals_separates_authorized_drop_from_revert() {
        use std::collections::HashSet;
        let prior = vec![
            json!({"id": "a", "status": "proposed"}),
            json!({"id": "b", "status": "proposed"}),
            json!({"id": "c", "status": "applied"}),
        ];
        // 'a' and 'c' removed; 'b' retained.
        let current = vec![json!({"id": "b", "status": "proposed"})];

        // Live drop auth covering 'a': authorized drop, no violation. 'c' was
        // applied, so its removal is allowed unconditionally (post-commit prune).
        let drop_ids: HashSet<String> = ["a".to_string()].into_iter().collect();
        let (dropped, violations) =
            classify_worklist_removals(&prior, &current, Some("drop"), &drop_ids);
        assert_eq!(dropped, ids(&["a"]));
        assert!(
            violations.is_empty(),
            "applied removals are never violations"
        );

        // No live auth — the consumed-auth state the false-success-drop race
        // produces. The same proposed 'a' removal is now an unauthorized
        // removal the watcher would revert. The last_worklist_cell claim in
        // handle_worklist_mutate prevents the watcher from ever reaching this
        // classification for an authorized drop.
        let empty: HashSet<String> = HashSet::new();
        let (dropped, violations) = classify_worklist_removals(&prior, &current, None, &empty);
        assert!(dropped.is_empty());
        assert_eq!(violations, vec![("a".to_string(), "proposed".to_string())]);
    }

    #[test]
    fn inflight_claim_fully_covered_requires_nonempty_and_all_covered() {
        let claimed = ids(&["a", "b"]);
        // Every claimed id present in mutated -> covered (clear + emit).
        assert!(inflight_claim_fully_covered(
            &claimed,
            &ids(&["a", "b", "c"])
        ));
        // Partial coverage -> not covered (don't clear the sentinel).
        assert!(!inflight_claim_fully_covered(&claimed, &ids(&["a"])));
        // Empty/absent claim -> not covered. This is the #133 skipped-resolve
        // case: no sentinel was written, so the mutate caller must emit its own
        // reconcile signal rather than relying on the clear.
        assert!(!inflight_claim_fully_covered(&ids(&[]), &ids(&["a"])));
    }

    #[test]
    fn resource_relative_path_joins_under_resources() {
        assert_eq!(
            resource_relative_path(Path::new("/tmp/bram"), "worklist.json"),
            Path::new("/tmp/bram")
                .join("resources")
                .join("worklist.json")
        );
        assert_eq!(
            resource_relative_path(Path::new("/tmp/bram"), Path::new("feedback-drafts")),
            Path::new("/tmp/bram")
                .join("resources")
                .join("feedback-drafts")
        );
    }

    #[test]
    fn draft_markdown_path_accepts_leaf_ids_only() {
        let dir = Path::new("/tmp/bram/resources/worklist-drafts");
        assert_eq!(
            draft_markdown_path(dir, "issue-145"),
            Some(dir.join("issue-145.md"))
        );
        assert_eq!(
            worklist_draft_path(dir, "issue-145"),
            Some(dir.join("issue-145.md"))
        );
        assert_eq!(
            feedback_draft_path(dir, "1780200000-issue-145"),
            Some(dir.join("1780200000-issue-145.md"))
        );
        assert_eq!(draft_markdown_path(dir, ""), None);
        assert_eq!(draft_markdown_path(dir, "../escape"), None);
        assert_eq!(draft_markdown_path(dir, "nested/item"), None);
        assert_eq!(draft_markdown_path(dir, "nested\\item"), None);
    }
}

#[cfg(test)]
mod project_config_tests {
    use super::{project_config_batch_commit_actions, ProjectConfig};

    fn flag(json: &str) -> bool {
        let config = serde_json::from_str::<ProjectConfig>(json).ok();
        project_config_batch_commit_actions(config)
    }

    #[test]
    fn batch_commit_actions_defaults_to_false() {
        assert!(flag(r#"{ "worklist": { "batchCommitActions": true } }"#));
        assert!(!flag(r#"{ "worklist": { "batchCommitActions": false } }"#));
        assert!(!flag(r#"{ "worklist": {} }"#));
        assert!(!flag(r#"{}"#));
        assert!(!flag(r#"{ "worklist": "#));
        assert!(!project_config_batch_commit_actions(None));
    }
}

fn handle_http<R: tauri::Runtime>(app: &AppHandle<R>, mut request: tiny_http::Request) {
    let url = request.url().to_string();
    let method = request.method().as_str().to_uppercase();
    let (raw_path, query) = match url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (url.as_str(), ""),
    };
    let path = raw_path.trim_start_matches('/');

    let route_correlation_id = next_route_correlation_id();
    let route_start = std::time::Instant::now();
    trace_route_entry(app, &method, path, query, &route_correlation_id);

    // POST-only routes (route_request is GET-only).
    let (status, content_type, body) = if path == "__worklist/mutate" {
        if method != "POST" {
            (405, "text/plain; charset=utf-8", b"POST only".to_vec())
        } else {
            let mut buf = Vec::new();
            let _ = request.as_reader().read_to_end(&mut buf);
            handle_worklist_mutate(app, &buf)
        }
    } else if path == "__iterate/begin" {
        if method != "POST" {
            (405, "text/plain; charset=utf-8", b"POST only".to_vec())
        } else {
            let mut buf = Vec::new();
            let _ = request.as_reader().read_to_end(&mut buf);
            handle_iterate_begin(app, &buf)
        }
    } else if path == "__iterate/end" || path == "__worklist/end" {
        // `/__worklist/end` is the alias agents call as the last action
        // of approved/drop turns (closing the cycle the resolve handler
        // opened by writing the sentinel). Both names route through the
        // same kind-agnostic handler — the sentinel doesn't care
        // whether it was written with kind:"approved", "drop", or
        // "iterate", and the clear logic only needs the id set.
        // Closes #91.
        if method != "POST" {
            (405, "text/plain; charset=utf-8", b"POST only".to_vec())
        } else {
            let mut buf = Vec::new();
            let _ = request.as_reader().read_to_end(&mut buf);
            handle_iterate_end(app, &buf)
        }
    } else if path == "__git/pull-rebase" {
        if method != "POST" {
            (405, "text/plain; charset=utf-8", b"POST only".to_vec())
        } else {
            handle_git_pull_rebase(app)
        }
    } else if path == "__settings" && method == "POST" {
        let mut buf = Vec::new();
        let _ = request.as_reader().read_to_end(&mut buf);
        handle_settings_post(app, &buf)
    } else if path == "__diff/annotate" && method == "POST" {
        let mut buf = Vec::new();
        let _ = request.as_reader().read_to_end(&mut buf);
        handle_diff_annotate(&buf)
    } else {
        route_request(app, path, query)
    };

    let body_size = body.len();
    trace_route_exit(
        app,
        &method,
        path,
        &route_correlation_id,
        status,
        body_size,
        route_start.elapsed().as_millis(),
    );

    let response = tiny_http::Response::from_data(body)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap(),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        )
        .with_header(
            // Internal API endpoints serve live state (sessions JSONL, git
            // status, etc.). Browser HTTP caching would defeat polling.
            tiny_http::Header::from_bytes(
                &b"Cache-Control"[..],
                &b"no-store, no-cache, must-revalidate"[..],
            )
            .unwrap(),
        );
    let _ = request.respond(response);
}

// Shell-document origin string used as the prefix for URLs that load
// in the shell or in same-origin iframes. Tauri 2 presents the scheme
// differently per platform:
//   - macOS / iOS / Linux: `tauri://localhost/...`
//   - Windows / Android:   `http://tauri.localhost/...` (HTTP localhost
//     subdomain — browser policy treats this as a secure context, so
//     service workers work; the macOS form is custom-scheme and not a
//     secure context, so SW does not work there)
// The scheme NAME we register against (`tauri`) is the same across
// platforms; only the URL form the WebView sees changes. The iframe
// `src` we hand to the JS side must match the platform's actual form
// so navigations resolve to our scheme handler rather than 404.
#[cfg(any(target_os = "windows", target_os = "android"))]
const SHELL_ORIGIN: &str = "http://tauri.localhost";

#[cfg(not(any(target_os = "windows", target_os = "android")))]
const SHELL_ORIGIN: &str = "tauri://localhost";

// Tauri custom-scheme handler that overrides Tauri's default tauri://
// behavior. Tauri 2 skips registering its built-in handler when an
// app-level handler with the same scheme name is present (see
// tauri-2.11.0/src/manager/webview.rs:267). With our handler in place:
//
//   - `tauri://localhost/__project/*` is proxied to the upstream URL in
//     PaneUrlsState.right_pane_upstream (the loopback HTTP server by
//     default, or an external project dev server when project config
//     declares one). The iframe's origin stays `tauri://localhost`, same
//     as the shell — same-origin policy then permits direct cross-frame
//     JS access. This is the whole point: shell and target can share
//     window globals (e.g., _xsLogs) without a postMessage bridge, and
//     the target reaches window.__TAURI__ directly.
//
//   - All other paths fall back to xd's own app/ tree via serve_app_file
//     (on-disk preferred, embedded fallback). Replicates Tauri's default
//     resource-loading behavior so the shell's own index.html, main.js,
//     vendor/*, __shell/*, __tools/* keep loading exactly as before.
//
// Hop-by-hop request and response headers are filtered out per RFC 7230.
// Connection failures to the upstream surface as 502 responses with a
// short error body so the iframe shows something rather than hanging.
fn handle_tauri_scheme<R: tauri::Runtime>(
    app: &AppHandle<R>,
    request: http::Request<Vec<u8>>,
) -> http::Response<Vec<u8>> {
    let uri = request.uri().clone();
    let path = uri.path();
    let rel = path.trim_start_matches('/');

    // Tier 1: /__project/* — project content escape hatch under the __
    // namespace. Strip prefix and proxy to right_pane_upstream (loopback
    // default or external dev server).
    if let Some(after) = rel.strip_prefix("__project/") {
        let upstream = {
            let state = app.state::<PaneUrlsState>();
            let urls = state.0.lock().unwrap();
            urls.right_pane_upstream.clone()
        };
        return proxy_to_target(upstream, after, uri.query(), request);
    }

    // Tier 2: shell assets from xd's app/ tree. Covers the shell's own
    // index.html, main.js, styles.css, vendor/*, and the tools pane at
    // tools/index.html + tools/components/*, tools/manual.md, etc.
    // Both panes can hit this directly via tauri://localhost/<path>.
    let app_rel = if rel.is_empty() { "index.html" } else { rel };
    if let Some((bytes, mime)) = serve_app_file(Some(app), app_rel) {
        return http::Response::builder()
            .status(200)
            .header("Content-Type", mime)
            .body(bytes)
            .unwrap_or_else(|_| {
                http::Response::builder()
                    .status(500)
                    .body(Vec::new())
                    .unwrap()
            });
    }

    // Tier 3: other /__* paths — xd-internal HTTP endpoints (sessions,
    // worklist, app-info, file, error, enhance, restart-server, etc.).
    // These always live on the loopback regardless of which project is
    // loaded; the loopback's own routing (route_request in lib.rs) knows
    // how to serve them. Includes the __vendor / __shell namespaces,
    // which the loopback maps to xd's app/vendor and app/__shell when
    // serve_app_file misses (e.g., for the `__vendor` -> `vendor`
    // prefix-strip mapping in the loopback's handler).
    if rel.starts_with("__") {
        let loopback = {
            let state = app.state::<PaneUrlsState>();
            let urls = state.0.lock().unwrap();
            urls.loopback_origin.clone()
        };
        return proxy_to_target(loopback, rel, uri.query(), request);
    }

    // Tier 4: everything else — project content at a non-`__*` absolute
    // path (e.g., /xmlui/foo.js for xmlui-weather, /Main.xmlui,
    // /resources/foo.svg). Proxy to right_pane_upstream so external dev
    // servers also work.
    let upstream = {
        let state = app.state::<PaneUrlsState>();
        let urls = state.0.lock().unwrap();
        urls.right_pane_upstream.clone()
    };
    proxy_to_target(upstream, rel, uri.query(), request)
}

fn proxy_to_target(
    upstream_base: String,
    path_after_origin: &str,
    query: Option<&str>,
    request: http::Request<Vec<u8>>,
) -> http::Response<Vec<u8>> {
    let mut url = format!("{}{}", upstream_base, path_after_origin);
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    proxy_to_upstream(url, request)
}

fn proxy_to_upstream(url: String, request: http::Request<Vec<u8>>) -> http::Response<Vec<u8>> {
    let method = request.method().clone();
    let (parts, body) = request.into_parts();

    let mut req = ureq::request(method.as_str(), &url);
    for (name, value) in parts.headers.iter() {
        let name_str = name.as_str();
        // Skip hop-by-hop headers and Host (ureq sets Host automatically
        // from the request URL — forwarding the shell's `tauri.localhost`
        // would confuse the upstream).
        if is_hop_by_hop(name_str) || name_str.eq_ignore_ascii_case("host") {
            continue;
        }
        if let Ok(value_str) = value.to_str() {
            req = req.set(name_str, value_str);
        }
    }

    let result = if body.is_empty() {
        req.call()
    } else {
        req.send_bytes(&body)
    };

    let response = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => {
            eprintln!("[scheme-proxy] {} {} -> {}", method, url, e);
            return http::Response::builder()
                .status(502)
                .header("Content-Type", "text/plain")
                .body(format!("upstream proxy error for {}: {}", url, e).into_bytes())
                .unwrap();
        }
    };

    let status = response.status();
    // Snapshot headers before consuming response into a reader.
    let header_names = response.headers_names();
    let header_pairs: Vec<(String, String)> = header_names
        .iter()
        .filter_map(|name| response.header(name).map(|v| (name.clone(), v.to_string())))
        .collect();

    let mut body_bytes = Vec::new();
    if let Err(e) = response.into_reader().read_to_end(&mut body_bytes) {
        eprintln!("[scheme-proxy] read error from {}: {}", url, e);
        return http::Response::builder()
            .status(502)
            .header("Content-Type", "text/plain")
            .body(format!("upstream read error: {}", e).into_bytes())
            .unwrap();
    }

    let mut builder = http::Response::builder().status(status);
    for (name, value) in header_pairs {
        if is_hop_by_hop(&name) {
            continue;
        }
        builder = builder.header(&name, &value);
    }
    builder.body(body_bytes).unwrap_or_else(|_| {
        http::Response::builder()
            .status(502)
            .body(Vec::new())
            .unwrap()
    })
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
// Bump RLIMIT_NOFILE on Unix so tiny_http's accept loop doesn't panic
// on EMFILE during long sessions. macOS default soft limit is often 256,
// which is too low once the filesystem watcher, iframe scheme-proxy,
// session-JSONL pollers, etc. accumulate. Target 8192 (or hard limit
// if lower). No-op on Windows — different FD semantics. See issue #44.
#[cfg(unix)]
fn raise_open_files_limit() {
    use libc::{getrlimit, rlimit, setrlimit, RLIMIT_NOFILE};
    unsafe {
        let mut current = rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if getrlimit(RLIMIT_NOFILE, &mut current) != 0 {
            eprintln!("[rlimit] getrlimit failed; not bumping");
            return;
        }
        let target: libc::rlim_t = 8192;
        let new_cur = std::cmp::min(target, current.rlim_max);
        if new_cur <= current.rlim_cur {
            eprintln!(
                "[rlimit] soft={} already meets target={}; not bumping",
                current.rlim_cur, target
            );
            return;
        }
        let new_limits = rlimit {
            rlim_cur: new_cur,
            rlim_max: current.rlim_max,
        };
        if setrlimit(RLIMIT_NOFILE, &new_limits) == 0 {
            eprintln!(
                "[rlimit] bumped soft FD limit {} -> {} (hard={})",
                current.rlim_cur, new_cur, current.rlim_max
            );
        } else {
            eprintln!(
                "[rlimit] setrlimit failed; staying at soft={}",
                current.rlim_cur
            );
        }
    }
}

#[cfg(not(unix))]
fn raise_open_files_limit() {}

pub fn run() {
    raise_open_files_limit();
    parse_cli_flags();
    init_bram_trace_from_env();
    let initial_proj = determine_project_root();
    eprintln!("[bram] project root: {}", initial_proj.display());
    if !initial_proj.join("index.html").exists() {
        eprintln!(
            "[bram] WARNING: no index.html at project root; the right pane will fail to load. Run with `bram /path/to/project` or cd into the project before launching."
        );
    }
    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("tauri", |ctx, request, responder| {
            // Offload to a thread so the WebView's request thread is not
            // blocked while the proxy fetches from the upstream. The
            // responder takes ownership and can be called from any thread.
            let app = ctx.app_handle().clone();
            std::thread::spawn(move || {
                let response = handle_tauri_scheme(&app, request);
                responder.respond(response);
            });
        })
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .manage(WhisperState::default())
        .manage(SpawnedServerState::default())
        .manage(ActiveProjectState(Mutex::new(initial_proj)))
        .manage(PaneUrlsState(Mutex::new(PaneUrls::default())))
        .invoke_handler(tauri::generate_handler![
            pty_spawn,
            pty_write,
            queue_pty_intent,
            queue_feedback_draft,
            pty_resize,
            log_from_right_pane,
            open_devtools,
            open_url,
            save_trace_export,
            capture_screenshot,
            git_push,
            get_right_pane_url,
            get_tools_pane_url,
            whisper_start,
            whisper_stop,
            whisper_status,
        ])
        .setup(move |app| {
            use tauri::Emitter;

            // Bind the right-pane HTTP server before anything else so the
            // URL is available the moment the parent shell asks for it.
            let server = tiny_http::Server::http("127.0.0.1:0")
                .map_err(|e| format!("failed to bind right-pane http server: {}", e))?;
            let port = server
                .server_addr()
                .to_ip()
                .map(|sa| sa.port())
                .ok_or("right-pane server bound to non-ip address")?;
            let _ = LOOPBACK_PORT.set(port);
            let started_at_ms = unix_now_ms();
            let _ = LOOPBACK_STARTED_MS.set(started_at_ms);
            let startup_project_root = project_root(Some(app.handle()));
            if let Some(proj) = startup_project_root.as_ref() {
                remove_bram_port_files(proj);
            }
            prepare_bram_trace_log(app.handle());
            if bram_trace_enabled() {
                append_bram_trace_line(
                    app.handle(),
                    "jsonl-turn-end",
                    "op=init providers=claude,codex codexMarker=event_msg/task_complete",
                );
            }
            // Remove any stale inflight sentinel from a prior session
            // that didn't complete cleanly. Refs #84.
            cleanup_stale_inflight_claim(app.handle());
            // Remove any stale Codex lifecycle intent/result files from a
            // prior session. Refs #130.
            cleanup_stale_worklist_intent(app.handle());
            // Remove any stale pty-intent queue from a prior session so
            // its intents don't replay into the fresh PTY. Refs #86.
            cleanup_stale_pty_intents(app.handle());
            let internal_origin = format!("http://127.0.0.1:{}", port);
            eprintln!("[bram] internal HTTP server: {}", internal_origin);
            // Tools pane lives at xd's app/tools/index.html, served via
            // the scheme handler's Tier 2 (shell asset) directly. Same
            // origin as the shell. SHELL_ORIGIN picks the right URL form
            // per platform — see the const definition above.
            let tools_url = format!("{}/tools/index.html", SHELL_ORIGIN);

            // .bram.json may declare an external server for the
            // right pane. The tools-pane URL always points at the internal
            // loopback (so the drawer keeps working regardless).
            let project_cfg = project_root(Some(app.handle()))
                .as_deref()
                .and_then(load_project_config);
            let default_right_pane = format!("{}/index.html", internal_origin);
            let internal_base = format!("{}/", internal_origin);
            // `right_pane` is the URL the iframe loads. With the
            // tauri:// scheme proxy, it's always a same-origin URL
            // under /__project/. `right_pane_upstream` is the bare
            // origin (http://host:port/) the scheme handler proxies
            // to; the configured `cfg.path` (including any query)
            // gets spliced into the iframe URL so the browser's own
            // relative-URL resolution produces clean sub-resource
            // paths that pass through the proxy unchanged.
            let (right_pane_url, right_pane_upstream) = if let Some(cfg) = project_cfg.as_ref().and_then(|c| c.server.as_ref()) {
                let external_origin = format!("http://localhost:{}/", cfg.port);
                let right_pane_external = {
                    let path = if cfg.path.starts_with('/') {
                        cfg.path.clone()
                    } else {
                        format!("/{}", cfg.path)
                    };
                    format!("{}/__project{}", SHELL_ORIGIN, path)
                };
                match probe_port_http(cfg.port, &cfg.path) {
                    PortStatus::Live => {
                        eprintln!(
                            "[server] port {} is live (HTTP responsive); reusing (skipping spawn of `{}`)",
                            cfg.port, cfg.command
                        );
                        (right_pane_external.clone(), external_origin.clone())
                    }
                    PortStatus::Unresponsive(reason) => {
                        eprintln!(
                            "[server] port {} is in use but unresponsive ({}); refusing to reuse",
                            cfg.port, reason
                        );
                        eprintln!(
                            "[server] HINT: a previous server is likely wedged. Run `lsof -i :{}` to find the pid, then kill it and restart Bram.",
                            cfg.port
                        );
                        // Surface the problem in the iframe via /__error
                        // on the internal loopback (the scheme handler
                        // proxies there; the iframe's URL stays under
                        // /__project so origin remains tauri://localhost).
                        let error_path = format!(
                            "__project/__error?reason={}",
                            percent_encode(&format!(
                                "Port {} is in use but unresponsive ({}). The previous Bram session likely left an orphan process. Run `lsof -i :{}` and kill the listed pid, then restart Bram.",
                                cfg.port, reason, cfg.port
                            ))
                        );
                        (
                            format!("{}/{}", SHELL_ORIGIN, error_path),
                            internal_base.clone(),
                        )
                    }
                    PortStatus::NotListening => {
                        if let Some(root) = project_root(Some(app.handle())) {
                            match spawn_project_server(cfg, &root) {
                                Ok(child) => {
                                    *app.state::<SpawnedServerState>().0.lock().unwrap() =
                                        Some(SpawnedServer {
                                            child,
                                            config: cfg.clone(),
                                        });
                                    if !wait_for_port(cfg.port, 5000) {
                                        eprintln!(
                                            "[server] WARNING: port {} did not come up within 5s; right-pane iframe will retry",
                                            cfg.port
                                        );
                                    } else {
                                        eprintln!("[server] port {} is up", cfg.port);
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[server] spawn failed: {} — falling back to internal URL", e);
                                }
                            }
                        }
                        (right_pane_external.clone(), external_origin.clone())
                    }
                }
            } else {
                (
                    format!("{}/__project/index.html", SHELL_ORIGIN),
                    internal_base.clone(),
                )
            };
            eprintln!("[bram] right pane URL: {}", right_pane_url);
            eprintln!("[bram] right pane upstream: {}", right_pane_upstream);
            eprintln!("[bram] tools pane URL: {}", tools_url);
            *app.state::<PaneUrlsState>().0.lock().unwrap() = PaneUrls {
                right_pane: right_pane_url,
                tools: tools_url,
                default_right_pane,
                right_pane_upstream,
                loopback_origin: internal_base.clone(),
            };

            let server_app = app.handle().clone();
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    let app = server_app.clone();
                    std::thread::spawn(move || handle_http(&app, request));
                }
            });
            // Publish the loopback port only after the accept loop is
            // actually serving HTTP. Otherwise agents can race startup:
            // lsof shows a listener, but curl still gets connection refused.
            if let Some(proj) = startup_project_root.as_ref() {
                let port_path = proj.join("resources/.bram-port");
                if wait_for_loopback_http(port, 5000) {
                    match write_bram_port_files(proj, port, started_at_ms) {
                        Ok(()) => eprintln!(
                            "[bram] wrote ready port files: {}, {}",
                            port_path.display(),
                            bram_port_metadata_path(&port_path).display()
                        ),
                        Err(e) => eprintln!(
                            "[bram] failed to write ready port files for {}: {}",
                            port_path.display(),
                            e
                        ),
                    }
                } else {
                    eprintln!(
                        "[bram] loopback port {} did not become HTTP-ready; leaving {} unpublished",
                        port,
                        port_path.display()
                    );
                }
            }

            let Some(proj_root) = project_root(Some(app.handle())) else {
                eprintln!("[watcher] could not resolve project root");
                return Ok(());
            };
            // Seed the worklist cache so the first detected change can
            // diff against the on-disk baseline rather than treating the
            // entire current file as "new".
            init_worklist_cache(app.handle());
            // Watch contract: events are emitted on two channels, NOT one.
            //   - "right-pane-reload" fires for changes inside proj_root only;
            //     main.js reloads the right-pane iframe alone. The agent
            //     tools drawer is poll-driven, so it does NOT need to reload
            //     when user-project files change. Keeping the drawer iframe
            //     stable here prevents postMessage-into-torn-down-iframe
            //     races on Approve/Drop clicks.
            //   - "tools-pane-reload" fires for changes under app/__shell,
            //     app/vendor, or app/tools; main.js reloads BOTH iframes
            //     (the drawer's own code changed, and the right pane may
            //     consume __shell/helpers.js too).
            // Do not collapse these back into a single event.
            let proj_root_path = proj_root.clone();
            let mut tools_pane_paths: Vec<std::path::PathBuf> = Vec::new();
            if let Some(app_root) = resolve_app_root(Some(app.handle())) {
                tools_pane_paths.push(app_root.join("__shell"));
                tools_pane_paths.push(app_root.join("vendor"));
                tools_pane_paths.push(app_root.join("tools"));
            } else {
                eprintln!("[watcher] no on-disk app/; using embedded tree (no app/ reload)");
            }
            // Provider session JSONLs get their own dispatch. Watch the
            // containing roots (not the file — the file rotates per session
            // and may not exist at startup) so the tools pane can refetch
            // immediately instead of waiting on fallback polling.
            let claude_sessions_dir = claude_sessions_dir(&app.handle()).ok();
            let codex_sessions_dir = home_dir().map(|h| h.join(".codex").join("sessions"));
            // Agent-hint file lives at <app_cache>/agent-hints/<encoded-cwd>.json
            // and is rewritten by the shell wrapper's _xmlui_mark_agent when
            // the user switches between `claude` and `codex`. Watching this
            // dir lets the agent-tools drawer refetch /__enhance/status when
            // the active provider flips.
            let agent_hints_dir = active_agent_hint_path(&app.handle())
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()));
            // ~/.bram/ holds the codex-trust-ack marker. Ensure it exists at
            // startup so the watcher can attach to it — without this, deleting
            // the marker (the documented "force the banner back" gesture) would
            // not trigger a refetch and the iframe would keep the stale state.
            let bram_dir = home_dir().map(|h| h.join(".bram"));
            if let Some(ref bd) = bram_dir {
                let _ = std::fs::create_dir_all(bd);
            }
            let mut watch_paths: Vec<std::path::PathBuf> = vec![proj_root_path.clone()];
            watch_paths.extend(tools_pane_paths.iter().cloned());
            if let Some(ref sd) = claude_sessions_dir {
                if sd.exists() {
                    watch_paths.push(sd.clone());
                }
            }
            if let Some(ref sd) = codex_sessions_dir {
                if sd.exists() {
                    watch_paths.push(sd.clone());
                }
            }
            if let Some(ref bd) = bram_dir {
                if bd.exists() {
                    watch_paths.push(bd.clone());
                }
            }
            if let Some(ref ah) = agent_hints_dir {
                if ah.exists() {
                    watch_paths.push(ah.clone());
                }
            }
            let app_handle = app.handle().clone();
            start_codex_session_poll_fallback(app_handle.clone());
            start_claude_turn_stats_poll(app_handle.clone());
            std::thread::spawn(move || {
                use notify::{recommended_watcher, Event, EventKind, RecursiveMode, Watcher};
                use std::sync::mpsc::channel;
                use std::time::{Duration, Instant};

                let (tx, rx) = channel::<notify::Result<Event>>();
                let mut watcher = match recommended_watcher(tx) {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("[watcher] init failed: {}", e);
                        return;
                    }
                };
                for watch_path in &watch_paths {
                    if let Err(e) = watcher.watch(watch_path, RecursiveMode::Recursive) {
                        eprintln!("[watcher] watch {:?} failed: {}", watch_path, e);
                        return;
                    }
                    eprintln!("[watcher] watching {:?}", watch_path);
                }

                let mut last_emit = Instant::now() - Duration::from_secs(1);
                let mut last_config_emit = Instant::now() - Duration::from_secs(1);
                // Debounce tools-pane reloads: defer the emit until 500ms
                // after the last tools-pane event, so a rapid burst of
                // saves coalesces into a single reload (single flash
                // instead of N). Other channels stay immediate.
                let tools_debounce = Duration::from_millis(500);
                let mut pending_tools_since: Option<Instant> = None;
                // Debounce worklist-changed emits: notify-rs produces ~6
                // events per atomic rename + modify of worklist.json
                // (and worklist-history snapshots), and the iframe's
                // worklist-changed listener triggers /__worklist and
                // /__worklist-history/list refetches per event. A 200ms
                // debounce coalesces the cascade into one emit per
                // logical change. Refs #85.
                let worklist_debounce = Duration::from_millis(200);
                let mut pending_worklist_since: Option<Instant> = None;
                use std::sync::mpsc::RecvTimeoutError;
                loop {
                    let res = rx.recv_timeout(Duration::from_millis(100));
                    // Always check the pending tools emit before processing
                    // the new event — a recv_timeout wake with no event is
                    // the typical trigger for firing a deferred reload.
                    if let Some(since) = pending_tools_since {
                        if since.elapsed() >= tools_debounce {
                            eprintln!("[watcher] change detected, emitting tools-pane-reload (debounced)");
                            emit_or_defer_tools_pane_reload(&app_handle);
                            pending_tools_since = None;
                        }
                    }
                    if let Some(since) = pending_worklist_since {
                        if since.elapsed() >= worklist_debounce {
                            eprintln!("[watcher] change detected, emitting worklist-changed (debounced)");
                            emit_replayable_signal(&app_handle, "worklist-changed");
                            pending_worklist_since = None;
                        }
                    }
                    let event = match res {
                        Ok(Ok(ev)) => ev,
                        Ok(Err(_)) => continue,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    let in_ignored_dir = |p: &std::path::Path| {
                        let igs = [".git", "target", "node_modules", "resources"];
                        p.components()
                            .any(|c| igs.iter().any(|ig| c.as_os_str() == *ig))
                    };

                    // [watcher] trace: one record per path per notify
                    // event, before any dispatch. Logs project-relative
                    // paths; absolute / outside-project paths fall back
                    // to file_name only so no host filesystem layout
                    // leaks into the log.
                    //
                    // Skip the live trace log and dated trace archives
                    // to avoid self-feeding loop / reload noise: the
                    // live file is written by append_bram_trace_line,
                    // and startup archiving may create
                    // resources/bram-traces/bram-trace-YYYY-MM-DD*.log files.
                    // Neither should emit more watcher trace or reload
                    // behavior. See fix-watcher-trace-self-feeding-loop.
                    if bram_trace_enabled() {
                        let change = notify_event_kind_label(&event.kind);
                        for p in &event.paths {
                            if p.starts_with(&proj_root_path) && in_ignored_dir(p) {
                                continue;
                            }
                            let rel = p
                                .strip_prefix(&proj_root_path)
                                .ok()
                                .map(|r| r.to_string_lossy().replace('\\', "/"))
                                .unwrap_or_else(|| {
                                    p.file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("")
                                        .to_string()
                                });
                            if rel == "resources/bram-traces/bram-trace.log"
                                || is_bram_trace_archive_rel(&rel)
                            {
                                continue;
                            }
                            append_bram_trace_line(
                                &app_handle,
                                "watcher",
                                &format!("path={} change={} dedup=false", rel, change),
                            );
                        }
                    }

                    // Session JSONL changes get their own dispatch. The
                    // Transcript / Workspace panes subscribe to
                    // talk-session-changed and refetch without waiting on
                    // the regular fallback poll interval.
                    let is_session_event = event.paths.iter().any(|p| {
                        p.extension().map_or(false, |e| e == "jsonl")
                            && (claude_sessions_dir.as_ref().map_or(false, |sd| p.starts_with(sd))
                                || codex_sessions_dir.as_ref().map_or(false, |sd| p.starts_with(sd)))
                    });
                    if is_session_event {
                        // [jsonl] trace: ground-truth signal that the
                        // agent is producing structured output. Lets
                        // #78 analysis tell a premature `[turn-end]`
                        // from a real one — a premature fire is
                        // followed within seconds by another `[jsonl]`
                        // line, proving the agent was still working.
                        // One trace line per jsonl path per watcher
                        // event (the filesystem batches at write-flush
                        // granularity, which is the cadence we want).
                        if bram_trace_enabled() {
                            for p in &event.paths {
                                if p.extension().map_or(false, |e| e == "jsonl") {
                                    let name = p
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("");
                                    let size =
                                        std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                                    append_bram_trace_line(
                                        &app_handle,
                                        "jsonl",
                                        &format!("file={} bytes={}", name, size),
                                    );
                                }
                            }
                        }
                        // JSONL-driven turn-end detection (#91 follow-up).
                        // Parses each changed JSONL for a Claude
                        // `stop_reason: "end_turn"` last-line marker; if
                        // present and the sentinel is claimed, clears it
                        // directly. More reliable than the PTY-silence
                        // path for cycles where the agent has multi-second
                        // pauses between bursts. The silence-driven path
                        // remains as fallback (gated by
                        // MIN_SILENCE_FOR_SENTINEL_CLEAR_MS); if this
                        // detector fires first, the silence-driven clear
                        // is a no-op because the sentinel is already gone.
                        for p in &event.paths {
                            if p.extension().map_or(false, |e| e == "jsonl") {
                                let mtime_ms = std::fs::metadata(p)
                                    .ok()
                                    .and_then(|md| md.modified().ok())
                                    .and_then(system_time_ms)
                                    .unwrap_or(0);
                                trace_jsonl_detector_handoff(
                                    &app_handle,
                                    "watcher",
                                    p,
                                    mtime_ms,
                                );
                                check_jsonl_for_turn_end(&app_handle, p);
                                // #170 follow-up: when a menu is currently
                                // shown without a signature, the cumulative
                                // recheck schedule (200 / 500 / 1500 / 3000
                                // ms) may still be too tight for very large
                                // tool_use payloads (e.g. `gh issue create`
                                // with multi-KB issue bodies). The watcher
                                // already sees JSONL grow on each Claude
                                // write; piggyback on that here for a
                                // dynamic retry the moment the file changes.
                                try_signature_recheck_on_jsonl_change(&app_handle);
                            }
                        }
                        // Removed the 100ms leading-edge debounce: it
                        // suppressed the FINAL write of an agent
                        // response burst (the one that flips
                        // isWaitingForAssistant to false), wedging the
                        // tools-pane spinner + disabled input until the
                        // next user activity. XMLUI dedupes refetches
                        // via structural sharing, so burst-emitting per
                        // event is fine.
                        emit_replayable_signal(&app_handle, "talk-session-changed");
                        continue;
                    }

                    // enhance-status (agent-setup state) changes when any
                    // of these files are touched. The tools-pane
                    // Main.xmlui listens for `enhance-status-changed` and
                    // refetches /__enhance/status, replacing the prior
                    // 2-second poll that ran while the user was typing.
                    // Not a `continue` — the regular tools-pane reload
                    // still fires on its normal path below.
                    let is_enhance_event = event.paths.iter().any(|p| {
                        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        let in_claude_dir = p.components().any(|c| c.as_os_str() == ".claude");
                        let in_agent_hints = agent_hints_dir
                            .as_ref()
                            .map_or(false, |ah| p.starts_with(ah));
                        let in_bram_dir = bram_dir.as_ref().map_or(false, |bd| p.starts_with(bd));
                        name == "CLAUDE.md"
                            || name == "AGENTS.md"
                            || (in_claude_dir
                                && (name == "settings.json"
                                    || name == "settings.local.json"
                                    || name == "worklist-guard.py"))
                            || in_agent_hints
                            || in_bram_dir
                    });
                    if is_enhance_event {
                        emit_replayable_signal(&app_handle, "enhance-status-changed");
                    }

                    // sessions-list-changed: any mutation under either
                    // Claude or Codex sessions dir — broader than
                    // talk-session-changed (which is JSONL-write-only).
                    // Sessions.xmlui listens for this to refetch its
                    // list (renames, deletes, new sessions all surface).
                    let is_sessions_list_event = event.paths.iter().any(|p| {
                        claude_sessions_dir
                            .as_ref()
                            .map_or(false, |sd| p.starts_with(sd))
                            || codex_sessions_dir
                                .as_ref()
                                .map_or(false, |sd| p.starts_with(sd))
                    });
                    if is_sessions_list_event {
                        emit_replayable_signal(&app_handle, "sessions-list-changed");
                    }

                    // worklist-changed: resources/worklist.json, any draft
                    // file, or anything under resources/worklist-history/. Workspace
                    // refetches its DataSources on this.
                    let is_worklist_change = event.paths.iter().any(|p| {
                        let in_resources = p.components().any(|c| c.as_os_str() == "resources");
                        let file = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        let in_history = p
                            .components()
                            .any(|c| c.as_os_str() == "worklist-history");
                        let in_drafts = p
                            .components()
                            .any(|c| c.as_os_str() == "worklist-drafts");
                        in_resources && (file == "worklist.json" || in_history || in_drafts)
                    });
                    if is_worklist_change {
                        // Defer the emit; pending_worklist_since either
                        // starts a fresh 200 ms window or rolls forward
                        // on a continuing burst. The loop's wake check
                        // above drains it when the window elapses. Refs
                        // #85 worklist-watcher-debounce.
                        pending_worklist_since = Some(Instant::now());
                    }

                    // feedback-history-changed: anything under
                    // resources/feedback-history/. The Feedback tab's
                    // DataSource refetches on this. Promotion is rare
                    // (once per iterate cycle), so no debounce needed.
                    let is_feedback_history_change = event.paths.iter().any(|p| {
                        p.components().any(|c| c.as_os_str() == "feedback-history")
                            && p.components().any(|c| c.as_os_str() == "resources")
                    });
                    if is_feedback_history_change {
                        emit_replayable_signal(&app_handle, "feedback-history-changed");
                    }

                    // Codex filesystem lifecycle drain (#130). When Codex
                    // writes resources/.worklist-intent.json, dispatch it
                    // through the same handlers the loopback routes use and
                    // write resources/.worklist-result.json. Fires on
                    // create/modify; the drain reads-then-deletes the intent
                    // file, so duplicate events in one notify burst no-op.
                    let is_intent_event = event.paths.iter().any(|p| {
                        p.file_name().and_then(|n| n.to_str()) == Some(".worklist-intent.json")
                            && p.components().any(|c| c.as_os_str() == "resources")
                    });
                    if is_intent_event
                        && matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
                    {
                        drain_worklist_intent(&app_handle);
                    }

                    // git-status-changed: any project file change that's
                    // not in the standard ignored directories. Commits
                    // refetches its log. Noisier than the others but
                    // bounded by the watcher's existing 100ms debounce.
                    let is_git_status_event = event.paths.iter().any(|p| {
                        p.starts_with(&proj_root_path) && !in_ignored_dir(p)
                    });
                    if is_git_status_event {
                        emit_replayable_signal(&app_handle, "git-status-changed");
                    }

                    // .bram.json gets its own dispatch: we have to
                    // process it on any event kind (editors atomic-save via
                    // rename, which arrives as Create or Remove rather than
                    // Modify), and its handler may need to respawn the
                    // project server before reloading the iframe.
                    let is_config_event = event.paths.iter().any(|p| {
                        is_project_config_path(&p)
                    });
                    if is_config_event {
                        if last_config_emit.elapsed() < Duration::from_millis(300) {
                            continue;
                        }
                        last_config_emit = Instant::now();
                        eprintln!("[project-config] change detected");
                        handle_project_config_reload(&app_handle, &proj_root_path);
                        continue;
                    }

                    if !matches!(
                        event.kind,
                        EventKind::Modify(_) | EventKind::Create(_)
                    ) {
                        continue;
                    }
                    // Worklist history capture. Worklist writes are otherwise
                    // skipped by the ignored-resources rule below (they
                    // shouldn't reload the iframe — the DataSource polls).
                    // Detect them here, snapshot the prior contents, then
                    // fall through to the normal skip.
                    let is_worklist_event = event.paths.iter().any(|p| {
                        let in_resources = p.components().any(|c| c.as_os_str() == "resources");
                        let in_drafts = p
                            .components()
                            .any(|c| c.as_os_str() == "worklist-drafts");
                        in_resources && (p.ends_with("worklist.json") || in_drafts)
                    });
                    if is_worklist_event {
                        maybe_snapshot_worklist(&app_handle);
                    }
                    // Skip events whose paths are entirely inside noisy or
                    // data-only directories. resources/ is data the DataSource
                    // polls; target/, .git/, node_modules/ are build/VCS noise.
                    if event.paths.iter().all(|p| {
                        p.starts_with(&proj_root_path) && in_ignored_dir(p)
                    }) {
                        continue;
                    }
                    // Editors often write twice (atomic save). Debounce 100ms.
                    if last_emit.elapsed() < Duration::from_millis(100) {
                        continue;
                    }
                    last_emit = Instant::now();
                    // Classify: any path under a tools_pane_paths root → tools event.
                    // Otherwise (paths only under proj_root) → right-pane-only event.
                    // Skip doc-only changes (e.g. conventions.md edits)
                    // from triggering a tools-pane-reload. They don't run
                    // code; the rebuild would only churn live UI state.
                    let is_tools_event = event.paths.iter().any(|p| {
                        let is_doc = p.extension().map_or(false, |e| e == "md");
                        !is_doc && tools_pane_paths.iter().any(|tp| p.starts_with(tp))
                    });
                    if is_tools_event {
                        // Defer the emit; pending_tools_since either starts
                        // the debounce window or resets it on burst writes.
                        pending_tools_since = Some(Instant::now());
                    } else {
                        eprintln!("[watcher] change detected, emitting right-pane-reload");
                        trace_emit_signal(&app_handle, "right-pane-reload");
                        let _ = app_handle.emit("right-pane-reload", ());
                    }
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                {
                    let state = app.state::<WhisperState>();
                    let mut guard = state.0.lock().unwrap();
                    if let Some(mut child) = guard.take() {
                        let pid = child.id();
                        let _ = child.kill();
                        let _ = child.wait();
                        eprintln!("[whisper] killed pid={} on exit", pid);
                    }
                }
                {
                    let state = app.state::<SpawnedServerState>();
                    let mut guard = state.0.lock().unwrap();
                    if let Some(mut spawned) = guard.take() {
                        let pid = spawned.child.id();
                        let _ = spawned.child.kill();
                        let _ = spawned.child.wait();
                        eprintln!("[server] killed pid={} on exit", pid);
                    }
                }
            }
        });
}
