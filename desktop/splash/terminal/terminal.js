// PTY window. Two modes determined by the URL fragment:
//
//   #__shell__   — interactive openclaw shell (cmd.exe / $SHELL with the
//                  `openclaw` wrapper on PATH). User can run any CLI subcommand.
//   #<provider>  — `openclaw onboard --auth-choice <provider>` for OAuth sign-in.
//
// Output (PTY → xterm) is broadcast via `pty://output`; keystrokes go back via
// the pty_input command. pty_exit fires once the child terminates.

(function () {
  const raw = decodeURIComponent((window.location.hash || "#").slice(1));
  const isShell = raw === "__shell__";
  const provider = raw || "anthropic";
  document.getElementById("provider").textContent = isShell ? "interactive shell" : provider;

  // Adjust the header for shell mode — different command, different intent.
  const headerEl = document.getElementById("header");
  if (isShell && headerEl) {
    headerEl.innerHTML =
      'openclaw shell &mdash; <strong>type <code>openclaw --help</code></strong> ' +
      '<span id="status"></span>';
  }

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
    if (statusEl) {
      statusEl.textContent = text;
      statusEl.className = cls || "";
    }
  }

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

  term.onData((data) => {
    invoke("pty_input", { input: data }).catch((err) => {
      term.write(`\r\n\x1b[31m[input error] ${err}\x1b[0m\r\n`);
    });
  });

  function syncSize() {
    fitAddon.fit();
    invoke("pty_resize", { rows: term.rows, cols: term.cols }).catch(() => {});
  }
  window.addEventListener("resize", syncSize);

  setStatus("· starting…");
  const startCall = isShell
    ? invoke("pty_start_shell", { rows: term.rows, cols: term.cols })
    : invoke("pty_start",       { provider, rows: term.rows, cols: term.cols });
  startCall
    .then(() => setStatus("· running…"))
    .catch((err) => {
      setStatus("· failed to start", "err");
      term.write(`\r\n\x1b[31m[start error] ${err}\x1b[0m\r\n`);
    });
})();
