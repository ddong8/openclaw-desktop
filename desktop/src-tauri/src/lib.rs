use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use tauri::{Manager, RunEvent};
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

fn openclaw_config_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(PathBuf::from(home).join(".openclaw").join("openclaw.json"))
}

fn generate_token_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    if getrandom::getrandom(&mut buf).is_err() {
        // Extremely unlikely on Win/macOS/Linux. Fall back to a time-seeded
        // value so the app still launches (token is loopback-only, never
        // crosses the network).
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

// Bootstrap a minimal openclaw.json on first launch so the Tauri webview can
// connect without pairing and without an interactive onboard wizard.
fn ensure_first_run_config() {
    let Some(path) = openclaw_config_path() else { return; };
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let token = generate_token_hex(24);
    let template = serde_json::json!({
        "gateway": {
            "mode": "local",
            "auth": { "mode": "token", "token": token },
            "port": 18789,
            "bind": "loopback",
            "controlUi": {
                "allowInsecureAuth": true,
                "dangerouslyDisableDeviceAuth": true
            }
        }
    });
    match serde_json::to_string_pretty(&template) {
        Ok(s) => {
            if let Err(err) = std::fs::write(&path, s) {
                eprintln!("[config] first-run write failed: {err}");
            } else {
                eprintln!("[config] wrote first-run openclaw.json (token + device-auth disabled)");
            }
        }
        Err(err) => eprintln!("[config] first-run serialize failed: {err}"),
    }
}

// Patch an existing openclaw.json so the Tauri webview can connect without
// per-device pairing approval. Idempotent.
fn ensure_disable_device_auth() {
    let Some(path) = openclaw_config_path() else { return; };
    if !path.exists() {
        return; // first-run bootstrap handles this
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(err) => { eprintln!("[config] read failed: {err}"); return; }
    };
    let mut v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => { eprintln!("[config] parse failed: {err}"); return; }
    };
    if !v.get("gateway").is_some_and(|x| x.is_object()) {
        v["gateway"] = serde_json::json!({});
    }
    if !v["gateway"].get("controlUi").is_some_and(|x| x.is_object()) {
        v["gateway"]["controlUi"] = serde_json::json!({});
    }
    let cu = &mut v["gateway"]["controlUi"];
    let already = cu.get("dangerouslyDisableDeviceAuth")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    if already {
        return;
    }
    cu["dangerouslyDisableDeviceAuth"] = serde_json::json!(true);
    match serde_json::to_string_pretty(&v) {
        Ok(s) => {
            if let Err(err) = std::fs::write(&path, s) {
                eprintln!("[config] write failed: {err}");
            } else {
                eprintln!("[config] patched gateway.controlUi.dangerouslyDisableDeviceAuth = true");
            }
        }
        Err(err) => eprintln!("[config] serialize failed: {err}"),
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

fn spawn_gateway(node_path: &PathBuf, openclaw_dir: &PathBuf) -> std::io::Result<Child> {
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

            ensure_first_run_config();
            ensure_disable_device_auth();

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
