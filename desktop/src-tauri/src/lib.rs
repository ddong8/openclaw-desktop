use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use std::process::{Child, Command};
use std::io::{Read, Write};

const GATEWAY_PORT: u16 = 18789;
const READINESS_TIMEOUT_SECS: u64 = 300; // OpenClaw cold-start can exceed 60s on first launch
const READINESS_POLL_MS: u64 = 500;

// Providers we expose in the tray "Sign in" submenu. Each entry pairs a CLI
// `--provider` id (passed straight to `openclaw models auth login`) with the
// human-readable label shown in the menu. OAuth-only providers go first; the
// list is intentionally short so the menu stays browsable — the user can pick
// "其他 provider…" to enter a custom id.
const OAUTH_PROVIDERS: &[(&str, &str)] = &[
    ("google-gemini-cli", "Google Gemini (CLI OAuth)"),
    ("openai-codex",      "OpenAI Codex (OAuth)"),
    ("claude-max",        "Claude Max (API proxy login)"),
    ("github-copilot",    "GitHub Copilot"),
    ("anthropic",         "Anthropic (API key / token)"),
    ("openai",            "OpenAI (API key)"),
];

struct PtySession {
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

#[derive(Default)]
struct AppState {
    sidecar: Mutex<Option<Child>>,
    port: Mutex<Option<u16>>,
    pty: Mutex<Option<PtySession>>,
    // Cached for spawn_login_pty so it doesn't have to redo path resolution.
    node_path: Mutex<Option<PathBuf>>,
    openclaw_dir: Mutex<Option<PathBuf>>,
}

#[tauri::command]
fn get_api_port(state: tauri::State<AppState>) -> Option<u16> {
    *state.port.lock().unwrap()
}

// ============================================================================
// PTY-backed "Sign in to provider" mini-terminal
//
// `openclaw models auth login` checks `process.stdin.isTTY` at the top of the
// command and exits with "requires an interactive TTY" on a plain pipe. To run
// OAuth flows (google-gemini-cli, openai-codex, claude-max, …) from our shell
// we allocate a real pseudo-terminal via portable-pty and bridge it to an
// xterm.js window. The window's terminal.js calls these commands.
// ============================================================================

#[tauri::command]
fn pty_start(
    provider: String,
    rows: u16,
    cols: u16,
    app: AppHandle,
    state: tauri::State<AppState>,
) -> Result<(), String> {
    // One PTY at a time. Kill any previous session before starting a new one.
    if let Some(prev) = state.pty.lock().unwrap().take() {
        let mut killer = prev.killer;
        let _ = killer.kill();
        drop(prev.writer);
        drop(prev.master);
    }

    let node_path = state.node_path.lock().unwrap().clone()
        .ok_or_else(|| "node path not initialized".to_string())?;
    let openclaw_dir = state.openclaw_dir.lock().unwrap().clone()
        .ok_or_else(|| "openclaw dir not initialized".to_string())?;

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| format!("openpty failed: {e}"))?;

    let mut cmd = portable_pty::CommandBuilder::new(&node_path);
    cmd.arg("openclaw.mjs");
    cmd.arg("models");
    cmd.arg("auth");
    cmd.arg("login");
    cmd.arg("--provider");
    cmd.arg(&provider);
    cmd.cwd(&openclaw_dir);
    // openclaw's clack prompter renders with truecolor + box-drawing chars; tell
    // node that stdout is a real terminal.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("FORCE_COLOR", "1");

    let mut child = pair.slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn failed: {e}"))?;
    // Close the slave handle on our side — the child still has it.
    drop(pair.slave);

    let reader = pair.master
        .try_clone_reader()
        .map_err(|e| format!("clone reader failed: {e}"))?;
    let writer = pair.master
        .take_writer()
        .map_err(|e| format!("take writer failed: {e}"))?;
    let killer = child.clone_killer();

    // Pump PTY -> Tauri events. The reader yields whatever bytes the child
    // wrote (ANSI escapes included); xterm.js handles rendering them.
    {
        let app = app.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: child closed stdout
                    Ok(n) => {
                        // Use lossy conversion — clack output is utf-8 but a
                        // chunk may split a multi-byte char; lossy replaces
                        // the partial with U+FFFD which xterm renders fine.
                        let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                        if app.emit("pty://output", serde_json::json!({ "data": chunk })).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Wait for child exit on a separate thread, then notify the window.
    {
        let app = app.clone();
        std::thread::spawn(move || {
            let status = child.wait();
            let code = match status {
                Ok(s) => s.exit_code() as i64,
                Err(_) => -1,
            };
            let _ = app.emit("pty://exit", serde_json::json!({ "code": code }));
        });
    }

    state.pty.lock().unwrap().replace(PtySession { writer, master: pair.master, killer });
    eprintln!("[pty] started openclaw models auth login --provider {provider}");
    Ok(())
}

#[tauri::command]
fn pty_input(input: String, state: tauri::State<AppState>) -> Result<(), String> {
    let mut guard = state.pty.lock().unwrap();
    let session = guard.as_mut().ok_or_else(|| "no active pty session".to_string())?;
    session.writer
        .write_all(input.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    session.writer.flush().map_err(|e| format!("flush failed: {e}"))?;
    Ok(())
}

#[tauri::command]
fn pty_resize(rows: u16, cols: u16, state: tauri::State<AppState>) -> Result<(), String> {
    let guard = state.pty.lock().unwrap();
    let Some(session) = guard.as_ref() else { return Ok(()); };
    session.master
        .resize(portable_pty::PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| format!("resize failed: {e}"))?;
    Ok(())
}

#[tauri::command]
fn pty_close(state: tauri::State<AppState>) -> Result<(), String> {
    let Some(prev) = state.pty.lock().unwrap().take() else { return Ok(()); };
    let mut killer = prev.killer;
    let _ = killer.kill();
    drop(prev.writer);
    drop(prev.master);
    Ok(())
}

fn open_login_terminal_window(app: &AppHandle, provider: &str) -> Result<(), tauri::Error> {
    // Reuse a single window across providers — closing the old one first kills
    // the old PTY too (via on_window_event below).
    if let Some(existing) = app.get_webview_window("login-terminal") {
        let _ = existing.close();
    }
    // Fragment over query string: Tauri's WebviewUrl::App treats the path as
    // a PathBuf and some backends strip the query, but fragments ride through
    // intact since they're never sent in the HTTP request line.
    let url = format!("terminal/index.html#{}", urlencoding_simple(provider));
    let app_for_close = app.clone();
    WebviewWindowBuilder::new(app, "login-terminal", WebviewUrl::App(url.into()))
        .title(format!("OpenClaw — Sign in to {provider}"))
        .inner_size(820.0, 520.0)
        .min_inner_size(640.0, 360.0)
        .resizable(true)
        .build()?;
    // When the window is closed by the user, drop the PTY so the openclaw
    // subprocess doesn't linger.
    if let Some(w) = app.get_webview_window("login-terminal") {
        w.on_window_event(move |event| {
            if matches!(event, tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed) {
                if let Some(state) = app_for_close.try_state::<AppState>() {
                    if let Some(prev) = state.pty.lock().unwrap().take() {
                        let mut killer = prev.killer;
                        let _ = killer.kill();
                        drop(prev.writer);
                        drop(prev.master);
                    }
                }
            }
        });
    }
    Ok(())
}

// Minimal URL encoder for provider ids (alphanumeric + '-'). All current
// OAuth provider ids are safe ASCII so this avoids pulling in a urlencoding
// crate just for one short string.
fn urlencoding_simple(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_string() } else { format!("%{:02X}", c as u32) })
        .collect()
}

// Read OpenClaw's npm package version from <openclaw_dir>/package.json so the
// tray menu can display it as a disabled item.
fn read_openclaw_version(openclaw_dir: &Path) -> String {
    let path = openclaw_dir.join("package.json");
    let Ok(s) = std::fs::read_to_string(&path) else { return "unknown".to_string(); };
    let v: serde_json::Value = match serde_json::from_str(&s) {
        Ok(v) => v,
        Err(_) => return "unknown".to_string(),
    };
    v.get("version").and_then(|x| x.as_str()).unwrap_or("unknown").to_string()
}

fn openclaw_config_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(PathBuf::from(home).join(".openclaw").join("openclaw.json"))
}

fn read_config_value() -> Option<serde_json::Value> {
    let path = openclaw_config_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn config_has_device_auth_disabled() -> bool {
    read_config_value()
        .and_then(|v| {
            v.get("gateway")
                .and_then(|g| g.get("controlUi"))
                .and_then(|c| c.get("dangerouslyDisableDeviceAuth"))
                .and_then(|x| x.as_bool())
        })
        .unwrap_or(false)
}

// gateway.mode must be present (e.g. "local") or `openclaw gateway` refuses to
// start with "existing config is missing gateway.mode". A config created by an
// older build (device-auth only) lacks it, so we must detect + repair.
fn config_has_gateway_mode() -> bool {
    read_config_value()
        .and_then(|v| {
            v.get("gateway")
                .and_then(|g| g.get("mode"))
                .and_then(|m| m.as_str())
                .map(|s| !s.is_empty())
        })
        .unwrap_or(false)
}

fn config_has_token() -> bool {
    read_gateway_token().is_some()
}

fn generate_token_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    if getrandom::getrandom(&mut buf).is_err() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ns = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((ns >> ((i % 16) * 4)) as u8) ^ (i as u8).wrapping_mul(31);
        }
    }
    let mut s = String::with_capacity(n_bytes * 2);
    for b in &buf { s.push_str(&format!("{b:02x}")); }
    s
}

// Run a one-shot openclaw CLI command with a hard timeout. Polls try_wait so a
// hung child (e.g. an unexpected interactive prompt in the TTY-less sidecar)
// can never block app startup — it gets killed at the deadline.
fn run_openclaw_cli(
    node_path: &std::path::Path,
    openclaw_dir: &std::path::Path,
    args: &[&str],
    stdin_data: Option<&str>,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let mut cmd = std::process::Command::new(node_path);
    cmd.arg("openclaw.mjs")
        .args(args)
        .current_dir(openclaw_dir)
        .stdin(std::process::Stdio::piped())
        // Inherit so `openclaw config patch` errors are visible when launched
        // from a terminal (helps diagnose first-run config failures).
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => { eprintln!("[cli] spawn `openclaw {args:?}` failed: {err}"); return None; }
    };

    // Write stdin (if any) and close it so the child doesn't wait for more input.
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(stdin_data.unwrap_or("").as_bytes());
        // drop(stdin) closes the pipe → EOF for the child
    }

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    eprintln!("[cli] `openclaw {args:?}` timed out after {timeout:?}; killing");
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => { eprintln!("[cli] try_wait failed: {err}"); return None; }
        }
    }
}

// Ensure ~/.openclaw/openclaw.json exists with a gateway token and
// device-auth disabled, so the Tauri webview connects with no pairing prompt.
//
// Uses a single `config patch --stdin` (≈1s, non-interactive, raw-roundtrip
// safe). We deliberately do NOT use `doctor --generate-gateway-token`: it runs
// slow system-service health checks (~60s) and renders interactive clack UI
// that hangs in the TTY-less sidecar — that was the "stuck on gateway startup"
// bug on fresh machines.
fn ensure_openclaw_config(node_path: &std::path::Path, openclaw_dir: &std::path::Path) {
    // All three must hold, else (re)apply. gateway.mode is critical: an older
    // build wrote device-auth + token but no gateway.mode, which makes
    // `openclaw gateway` refuse to start and hang the splash forever.
    if config_has_gateway_mode() && config_has_device_auth_disabled() && config_has_token() {
        return; // warm start — already fully configured, zero latency
    }

    let token = read_gateway_token().unwrap_or_else(|| generate_token_hex(24));
    let patch = format!(
        r#"{{"gateway":{{"mode":"local","auth":{{"mode":"token","token":"{token}"}},"port":18789,"bind":"loopback","controlUi":{{"dangerouslyDisableDeviceAuth":true,"allowInsecureAuth":true}}}}}}"#
    );
    eprintln!("[config] applying gateway config via `config patch` (non-interactive)");
    let status = run_openclaw_cli(
        node_path,
        openclaw_dir,
        &["config", "patch", "--stdin"],
        Some(&patch),
        Duration::from_secs(60),
    );
    match status {
        Some(s) if s.success() => eprintln!("[config] config patch applied"),
        Some(s) => eprintln!("[config] config patch exited with {s:?}"),
        None => eprintln!("[config] config patch failed/timed out — gateway may require manual setup"),
    }
}

fn read_gateway_token() -> Option<String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let path = PathBuf::from(home).join(".openclaw").join("openclaw.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("gateway")
        .and_then(|g| g.get("auth"))
        .and_then(|a| a.get("token"))
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

fn resolve_sidecar_paths(app: &tauri::AppHandle) -> tauri::Result<(PathBuf, PathBuf)> {
    // node_path and openclaw_dir are kept as PathBuf for ergonomic .clone()
    // in async closures.
    let node_exe = if cfg!(windows) { "node.exe" } else { "node" };

    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(rd) = app.path().resource_dir() {
        // Tauri sometimes reports the install root, sometimes the resources subdir.
        candidates.push(rd.clone());
        candidates.push(rd.join("resources"));
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("resources"));
            candidates.push(dir.to_path_buf());
        }
    }

    // Dev fallback — env! is compile-time, only helps the build machine.
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources"));

    for base in &candidates {
        let n = base.join("node").join(node_exe);
        if n.exists() {
            return Ok((n, base.join("openclaw")));
        }
    }

    eprintln!("[sidecar] could not locate node binary in any candidate:");
    for c in &candidates {
        eprintln!("[sidecar]   - {}", c.display());
    }
    let fallback = candidates.first().cloned().unwrap_or_default();
    Ok((fallback.join("node").join(node_exe), fallback.join("openclaw")))
}

fn spawn_gateway(node_path: &Path, openclaw_dir: &Path) -> std::io::Result<Child> {
    let mut cmd = Command::new(node_path);
    cmd.arg("openclaw.mjs")
        .arg("gateway")
        .arg("--port")
        .arg(GATEWAY_PORT.to_string())
        .current_dir(openclaw_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW = 0x08000000 — hide the child console window.
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    cmd.spawn()
}

async fn wait_for_ready() -> bool {
    let url = format!("http://127.0.0.1:{}/", GATEWAY_PORT);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(READINESS_TIMEOUT_SECS);
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = client.get(&url).send().await {
            // Gateway may answer 200 (Control UI), 401 (auth required), or 426 (upgrade).
            // Any non-5xx response means it's listening.
            let status = resp.status().as_u16();
            if status < 500 {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(READINESS_POLL_MS)).await;
    }
    false
}

// Check GitHub Releases for a newer signed build; prompt the user, then
// download + install + restart. Runs best-effort: any failure (offline,
// no update, GitHub unreachable) is logged and ignored.
async fn check_for_update(app: tauri::AppHandle) {
    use tauri_plugin_updater::UpdaterExt;

    let updater = match app.updater() {
        Ok(u) => u,
        Err(err) => { eprintln!("[updater] init failed: {err}"); return; }
    };
    let update = match updater.check().await {
        Ok(Some(u)) => u,
        Ok(None) => { eprintln!("[updater] already up to date"); return; }
        Err(err) => { eprintln!("[updater] check failed: {err}"); return; }
    };

    let new_ver = update.version.clone();
    let cur_ver = update.current_version.clone();
    eprintln!("[updater] update available: {cur_ver} -> {new_ver}");

    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let approved = app
        .dialog()
        .message(format!(
            "发现新版本 {new_ver}(当前 {cur_ver})。\n现在下载并更新?更新完成后应用会自动重启。"
        ))
        .title("OpenClaw 有可用更新")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom("立即更新".to_string(), "稍后".to_string()))
        .blocking_show();

    if !approved {
        eprintln!("[updater] user postponed update");
        return;
    }

    match update.download_and_install(|_downloaded, _total| {}, || {}).await {
        Ok(_) => {
            eprintln!("[updater] installed {new_ver}; restarting");
            app.restart();
        }
        Err(err) => {
            eprintln!("[updater] download/install failed: {err}");
            let _ = app
                .dialog()
                .message(format!("更新失败:{err}\n可前往 GitHub Releases 手动下载。"))
                .title("更新失败")
                .kind(MessageDialogKind::Error)
                .blocking_show();
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            get_api_port,
            pty_start,
            pty_input,
            pty_resize,
            pty_close,
        ])
        .on_window_event(|window, event| {
            // Hide the window on close instead of quitting — the embedded
            // OpenClaw gateway must stay running so channel webhooks
            // (WeChat / Telegram / etc.) keep receiving messages. The user
            // exits the app explicitly via the tray "退出" menu item.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            let app_handle = app.handle().clone();
            let (node_path, openclaw_dir) = resolve_sidecar_paths(&app_handle)?;

            if !node_path.exists() {
                eprintln!("[sidecar] node binary not found at {:?}", node_path);
                return Err(format!("node binary missing: {}", node_path.display()).into());
            }
            if !openclaw_dir.join("openclaw.mjs").exists() {
                eprintln!("[sidecar] openclaw.mjs not found in {:?}", openclaw_dir);
                return Err(format!("openclaw entry missing in: {}", openclaw_dir.display()).into());
            }

            ensure_openclaw_config(&node_path, &openclaw_dir);

            // Cache for pty_start to spawn the openclaw subprocess without
            // re-doing path resolution.
            {
                let state = app_handle.state::<AppState>();
                *state.node_path.lock().unwrap() = Some(node_path.clone());
                *state.openclaw_dir.lock().unwrap() = Some(openclaw_dir.clone());
            }

            // ---- Tray icon + menu ----
            {
                let oc_version = read_openclaw_version(&openclaw_dir);
                let shell_version = env!("CARGO_PKG_VERSION");
                let about_label = format!("OpenClaw v{oc_version} (shell {shell_version})");

                // Per-provider sign-in submenu. Each item id is "login:<provider>"
                // so the click handler can route on prefix.
                let mut provider_items: Vec<MenuItem<_>> = Vec::with_capacity(OAUTH_PROVIDERS.len());
                for (id, label) in OAUTH_PROVIDERS {
                    provider_items.push(MenuItem::with_id(
                        app,
                        format!("login:{id}"),
                        *label,
                        true,
                        None::<&str>,
                    )?);
                }
                let login_submenu = Submenu::with_id_and_items(
                    app,
                    "login_menu",
                    "登录 provider…",
                    true,
                    &provider_items.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<_>).collect::<Vec<_>>(),
                )?;

                let menu = Menu::with_items(app, &[
                    &MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &login_submenu,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "open_config", "打开配置文件", true, None::<&str>)?,
                    &MenuItem::with_id(app, "open_data", "打开配置目录", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "restart", "重启服务", true, None::<&str>)?,
                    &MenuItem::with_id(app, "upgrade", "升级 OpenClaw", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "about", &about_label, false, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?,
                ])?;

                let mut tray_builder = TrayIconBuilder::with_id("main-tray")
                    .menu(&menu)
                    .show_menu_on_left_click(false)
                    .tooltip("OpenClaw")
                    .on_tray_icon_event(|tray, event| {
                        // Left-click → restore + focus the main window.
                        // Right-click is handled by the menu (show_menu_on_left_click=false).
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                        }
                    })
                    .on_menu_event(move |app, event| {
                        let id = event.id.as_ref();
                        match id {
                            "show" => {
                                if let Some(w) = app.get_webview_window("main") {
                                    let _ = w.show();
                                    let _ = w.unminimize();
                                    let _ = w.set_focus();
                                }
                            }
                            "open_config" => {
                                if let Some(path) = openclaw_config_path() {
                                    use tauri_plugin_opener::OpenerExt;
                                    let s = path.to_string_lossy().into_owned();
                                    if let Err(err) = app.opener().open_path(s, None::<&str>) {
                                        eprintln!("[tray] open_config failed: {err}");
                                    }
                                }
                            }
                            "open_data" => {
                                if let Some(path) = openclaw_config_path().and_then(|p| p.parent().map(|x| x.to_path_buf())) {
                                    use tauri_plugin_opener::OpenerExt;
                                    let s = path.to_string_lossy().into_owned();
                                    if let Err(err) = app.opener().open_path(s, None::<&str>) {
                                        eprintln!("[tray] open_data failed: {err}");
                                    }
                                }
                            }
                            "restart" => {
                                eprintln!("[tray] restart requested — relaunching app");
                                app.restart();
                            }
                            "upgrade" => {
                                // Check GitHub Releases via tauri-plugin-updater
                                // (downloads a newer signed build of the whole app).
                                eprintln!("[tray] manual update check requested");
                                tauri::async_runtime::spawn(check_for_update(app.clone()));
                            }
                            "quit" => {
                                app.exit(0);
                            }
                            other if other.starts_with("login:") => {
                                let provider = &other["login:".len()..];
                                if let Err(err) = open_login_terminal_window(app, provider) {
                                    eprintln!("[tray] open login terminal failed: {err}");
                                }
                            }
                            _ => {}
                        }
                    });

                // Reuse the app window icon (the lobster).
                if let Some(icon) = app.default_window_icon() {
                    tray_builder = tray_builder.icon(icon.clone());
                }
                tray_builder.build(app)?;
            }

            let mut child = spawn_gateway(&node_path, &openclaw_dir)?;

            // Drain stdout / stderr on plain OS threads (the child is a
            // std::process::Child — no Tokio runtime needed; tokio::process on
            // Unix requires a reactor that isn't present in the setup hook).
            if let Some(stdout) = child.stdout.take() {
                std::thread::spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                        eprintln!("[sidecar:out] {line}");
                    }
                });
            }
            if let Some(stderr) = child.stderr.take() {
                std::thread::spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                        eprintln!("[sidecar:err] {line}");
                    }
                });
            }

            app_handle
                .state::<AppState>()
                .sidecar
                .lock()
                .unwrap()
                .replace(child);

            // Create the main window programmatically so we can install:
            //  - a navigation handler that opens external links in the system browser
            //  - an initialization script that intercepts in-page clicks and window.open
            //    (Control UI's anchors/popups don't trigger the main-frame nav handler)
            let opener_handle = app_handle.clone();
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("OpenClaw")
            .inner_size(1280.0, 800.0)
            .min_inner_size(800.0, 600.0)
            .resizable(true)
            .initialization_script(concat!(
                "console.log('[openclaw-desktop] init script starting', {tauri:!!window.__TAURI__, opener:!!(window.__TAURI__&&window.__TAURI__.opener), internals:!!window.__TAURI_INTERNALS__});",
                include_str!("../scripts/external-link-hook.js"),
                "console.log('[openclaw-desktop] external-link-hook installed');"
            ))
            .on_navigation(move |url| {
                let scheme = url.scheme();
                let host = url.host_str().unwrap_or("");
                // Tauri 2 on Windows serves the embedded frontend from
                // http://tauri.localhost/ (and IPC from http://ipc.localhost/),
                // so those must NOT be treated as external.
                let is_loopback_host = matches!(host, "127.0.0.1" | "localhost")
                    || host.ends_with(".localhost");
                let allow_internal = matches!(scheme, "tauri" | "about" | "data" | "blob" | "file")
                    || (matches!(scheme, "http" | "https" | "ws" | "wss") && is_loopback_host);
                if allow_internal {
                    return true;
                }
                use tauri_plugin_opener::OpenerExt;
                if let Err(err) = opener_handle.opener().open_url(url.as_str(), None::<&str>) {
                    eprintln!("[opener] failed to open external url {url}: {err}");
                }
                false
            })
            .build()?;

            #[cfg(debug_assertions)]
            if let Some(w) = app_handle.get_webview_window("main") {
                w.open_devtools();
            }

            let ready_handle = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                if wait_for_ready().await {
                    *ready_handle.state::<AppState>().port.lock().unwrap() = Some(GATEWAY_PORT);
                    eprintln!("[sidecar] gateway ready on port {}", GATEWAY_PORT);
                    if let Some(window) = ready_handle.get_webview_window("main") {
                        let url = match read_gateway_token() {
                            Some(token) => format!(
                                "http://127.0.0.1:{}/#token={}",
                                GATEWAY_PORT, token
                            ),
                            None => {
                                eprintln!("[sidecar] no gateway token in ~/.openclaw/openclaw.json — UI will require manual auth");
                                format!("http://127.0.0.1:{}/", GATEWAY_PORT)
                            }
                        };
                        let script = format!("window.location.replace({});", serde_json::to_string(&url).unwrap());
                        if let Err(err) = window.eval(&script) {
                            eprintln!("[sidecar] webview navigate failed: {err}");
                        }
                    } else {
                        eprintln!("[sidecar] webview window 'main' not found");
                    }
                } else {
                    eprintln!(
                        "[sidecar] gateway readiness timed out after {}s",
                        READINESS_TIMEOUT_SECS
                    );
                }
            });

            // Auto-check for app updates ~15s after launch (best-effort, prompts on found).
            #[cfg(not(debug_assertions))]
            {
                let update_handle = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    check_for_update(update_handle).await;
                });
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri application")
        .run(|app_handle, event| {
            if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    if let Some(mut child) = state.sidecar.lock().unwrap().take() {
                        let _ = child.kill();
                    }
                    if let Some(pty) = state.pty.lock().unwrap().take() {
                        let mut killer = pty.killer;
                        let _ = killer.kill();
                        drop(pty.writer);
                        drop(pty.master);
                    }
                }
            }
        });
}

// icon refreshed: pixel-lobster
// rebuild trigger 1779344215
