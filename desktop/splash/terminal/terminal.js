// Mini terminal window: wires xterm.js to a PTY-spawned `openclaw models auth
// login --provider <X>` running inside the embedded Node sidecar. Backed by
// portable-pty in the Rust shell — interactive prompts (clack), arrow-key
// menus, and the OAuth callback URL all render here. When the subprocess
// exits, the window stays open briefly so the user can read the final lines.

(function () {
  // Provider id comes in via the URL fragment (#<provider>) — fragments
  // survive Tauri's PathBuf-based WebviewUrl::App resolution more reliably
  // than query strings on all three platforms.
  const provider = decodeURIComponent((window.location.hash || "#").slice(1)) || "anthropic";
  document.getElementById("provider").textContent = provider;

  const term = new Terminal({
    fontFamily: 'Menlo, "Cascadia Code", "Consolas", monospace',
    fontSize: 13,
    theme: {
      background: "#0b0f14",
      foreground: "#d5dde7",
      cursor: "#6cd06c",
      selectionBackground: "rgba(108, 208, 108, 0.25)",
    },
    cursorBlink: true,
    convertEol: true,
    scrollback: 5000,
  });
  const fitAddon = new FitAddon.FitAddon();
  term.loadAddon(fitAddon);
  term.open(document.getElementById("term"));
  fitAddon.fit();

  const tauri = window.__TAURI__;
  if (!tauri || !tauri.core || !tauri.event) {
    term.write("\r\n\x1b[31m[error] Tauri runtime not available — this page can only be opened from the OpenClaw app.\x1b[0m\r\n");
    return;
  }
  const invoke = tauri.core.invoke;
  const listen = tauri.event.listen;

  const statusEl = document.getElementById("status");
  function setStatus(text, cls) {
    statusEl.textContent = text;
    statusEl.className = cls || "";
  }

  // Pump output: Rust emits `pty://output` { data: string (utf-8 chunk) }.
  listen("pty://output", (e) => {
    if (e && e.payload && typeof e.payload.data === "string") {
      term.write(e.payload.data);
    }
  });

  listen("pty://exit", (e) => {
    const code = (e && e.payload && e.payload.code) ?? null;
    const ok = code === 0;
    setStatus(ok ? `· exited (code 0)` : `· exited (code ${code})`, ok ? "ok" : "err");
    term.write(`\r\n\x1b[2m[${ok ? "done" : "exited code " + code}]\x1b[0m\r\n`);
  });

  // Send input: keyboard → Rust writes to PTY stdin.
  term.onData((data) => {
    invoke("pty_input", { input: data }).catch((err) => {
      term.write(`\r\n\x1b[31m[input error] ${err}\x1b[0m\r\n`);
    });
  });

  // Resize PTY when the window/terminal viewport changes (xterm computes
  // rows/cols from the container, FitAddon syncs them).
  function syncSize() {
    fitAddon.fit();
    invoke("pty_resize", { rows: term.rows, cols: term.cols }).catch(() => {});
  }
  window.addEventListener("resize", syncSize);

  // Kick off the PTY-backed openclaw subprocess. Rust window-shows + invokes
  // this command once the window is ready.
  setStatus("· starting…");
  invoke("pty_start", { provider, rows: term.rows, cols: term.cols })
    .then(() => setStatus("· running…"))
    .catch((err) => {
      setStatus("· failed to start", "err");
      term.write(`\r\n\x1b[31m[start error] ${err}\x1b[0m\r\n`);
    });

  // When the user closes the window, Rust drops the PTY which kills the child.
  // Nothing extra needed here.
})();
