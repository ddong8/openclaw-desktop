use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{Manager, RunEvent};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tokio::process::{Child, Command};
use tokio::io::{AsyncBufReadExt, BufReader};

const GATEWAY_PORT: u16 = 18789;
const READINESS_TIMEOUT_SECS: u64 = 300; // OpenClaw cold-start can exceed 60s on first launch
const READINESS_POLL_MS: u64 = 500;

#[derive(Default)]
struct AppState {
    sidecar: Mutex<Option<Child>>,
    port: Mutex<Option<u16>>,
}

#[tauri::command]
fn get_api_port(state: tauri::State<AppState>) -> Option<u16> {
    *state.port.lock().unwrap()
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

fn config_has_device_auth_disabled() -> bool {
    let Some(path) = openclaw_config_path() else { return false; };
    let Ok(content) = std::fs::read_to_string(&path) else { return false; };
    let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(&content) else { return false; };
    v.get("gateway")
        .and_then(|g| g.get("controlUi"))
        .and_then(|c| c.get("dangerouslyDisableDeviceAuth"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

fn config_has_token() -> bool {
    read_gateway_token().is_some()
}

// Run a one-shot openclaw CLI command synchronously, inheriting stdin/stdout.
// Returns the exit status of the child or None on spawn failure.
fn run_openclaw_cli(
    node_path: &std::path::Path,
    openclaw_dir: &std::path::Path,
    args: &[&str],
    stdin_data: Option<&str>,
) -> Option<std::process::ExitStatus> {
    let mut cmd = std::process::Command::new(node_path);
    cmd.arg("openclaw.mjs").args(args).current_dir(openclaw_dir);

    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW = 0x08000000 — hide the console window
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    if let Some(input) = stdin_data {
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => { eprintln!("[cli] spawn `openclaw {args:?}` failed: {err}"); return None; }
        };
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(input.as_bytes());
        }
        match child.wait() {
            Ok(s) => Some(s),
            Err(err) => { eprintln!("[cli] wait failed: {err}"); None }
        }
    } else {
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        match cmd.status() {
            Ok(s) => Some(s),
            Err(err) => { eprintln!("[cli] run `openclaw {args:?}` failed: {err}"); None }
        }
    }
}

// Ensure ~/.openclaw/openclaw.json exists AND has
// gateway.controlUi.dangerouslyDisableDeviceAuth=true / allowInsecureAuth=true.
//
// We use OpenClaw's own CLI (`doctor --generate-gateway-token`, `config patch
// --stdin`) so OpenClaw owns the serialized representation and the Control UI's
// Raw editor stays available (it disables itself when the config can't safely
// round-trip raw text).
fn ensure_openclaw_config(node_path: &std::path::Path, openclaw_dir: &std::path::Path) {
    let path = match openclaw_config_path() {
        Some(p) => p,
        None => return,
    };

    let device_auth_ok = config_has_device_auth_disabled();
    let token_ok = config_has_token();
    if path.exists() && device_auth_ok && token_ok {
        return;
    }

    // First-run case: ask OpenClaw to generate its own gateway token + base config.
    if !path.exists() || !token_ok {
        eprintln!("[config] bootstrapping via `openclaw doctor --generate-gateway-token`");
        let status = run_openclaw_cli(
            node_path,
            openclaw_dir,
            &["doctor", "--generate-gateway-token", "--non-interactive"],
            None,
        );
        if let Some(s) = status {
            if !s.success() {
                eprintln!("[config] doctor exited with {s:?}");
            }
        }
    }

    // Apply our device-auth overrides via patch (validated, raw-safe).
    if !config_has_device_auth_disabled() {
        eprintln!("[config] patching gateway.controlUi via `openclaw config patch`");
        let patch = r#"{"gateway":{"controlUi":{"dangerouslyDisableDeviceAuth":true,"allowInsecureAuth":true}}}"#;
        let status = run_openclaw_cli(node_path, openclaw_dir, &["config", "patch", "--stdin"], Some(patch));
        if let Some(s) = status {
            if !s.success() {
                eprintln!("[config] config patch exited with {s:?}");
            }
        }
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
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

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

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![get_api_port])
        .on_window_event(|window, event| {
            // Forward webview popup requests (window.open / target="_blank") to system browser.
            // Tauri does this via the navigation handler below, but we leave this hook in place
            // for any future window-level signals.
            let _ = (window, event);
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

            // ---- Tray icon + menu ----
            {
                let oc_version = read_openclaw_version(&openclaw_dir);
                let shell_version = env!("CARGO_PKG_VERSION");
                let about_label = format!("OpenClaw v{oc_version} (shell {shell_version})");

                let menu = Menu::with_items(app, &[
                    &MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?,
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

                let node_for_menu = node_path.clone();
                let openclaw_for_menu = openclaw_dir.clone();

                let mut tray_builder = TrayIconBuilder::with_id("main-tray")
                    .menu(&menu)
                    .show_menu_on_left_click(false)
                    .tooltip("OpenClaw")
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
                                let node = node_for_menu.clone();
                                let dir = openclaw_for_menu.clone();
                                std::thread::spawn(move || {
                                    eprintln!("[tray] running `openclaw update --yes` in background…");
                                    let status = run_openclaw_cli(&node, &dir, &["update", "--yes"], None);
                                    eprintln!("[tray] update exited: {status:?}");
                                });
                            }
                            "quit" => {
                                app.exit(0);
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

            // Drain stdout / stderr to keep the buffer from filling up and to surface errors.
            if let Some(stdout) = child.stdout.take() {
                tauri::async_runtime::spawn(async move {
                    let mut reader = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = reader.next_line().await {
                        eprintln!("[sidecar:out] {}", line);
                    }
                });
            }
            if let Some(stderr) = child.stderr.take() {
                tauri::async_runtime::spawn(async move {
                    let mut reader = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = reader.next_line().await {
                        eprintln!("[sidecar:err] {}", line);
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

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri application")
        .run(|app_handle, event| {
            if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    if let Some(mut child) = state.sidecar.lock().unwrap().take() {
                        let _ = child.start_kill();
                    }
                }
            }
        });
}

// icon refreshed: pixel-lobster
// rebuild trigger 1779344215
