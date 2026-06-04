// Splash window. Rust (lib.rs) emits `splash://stage` events as the sidecar
// boots: spawn (10%) → loaded (40%) → migrating (65%) → waiting-port (85%)
// → ready (100%). Rust navigates the window to the Control UI once ready, so
// `ready` is the last event we ever see here.
(function () {
  const status = document.getElementById('status');
  const elapsed = document.getElementById('elapsed');
  const bar = document.getElementById('bar');
  const error = document.getElementById('error');
  const startedAt = Date.now();

  // Tick a "已运行 Xs" footer so the user always sees something moving even if
  // a stage gets stuck for a while.
  setInterval(function () {
    const secs = Math.floor((Date.now() - startedAt) / 1000);
    if (secs >= 2) elapsed.textContent = '已运行 ' + secs + ' s';
    if (secs >= 300) {
      // 5 min timeout — show the diagnostic hint.
      elapsed.hidden = true;
      bar.parentElement.hidden = true;
      status.hidden = true;
      error.hidden = false;
      error.textContent =
        '5 分钟内 Gateway 没有启动。可能原因:\n' +
        '  - 端口 18789 被占用\n' +
        '  - 嵌入的 Node.js 或 openclaw 解压失败\n' +
        '从终端启动看 stderr 可定位具体阶段。';
    }
  }, 1000);

  function applyStage(percent, detail) {
    if (typeof percent === 'number') {
      bar.style.width = Math.max(0, Math.min(100, percent)) + '%';
      if (percent >= 100) bar.classList.add('done');
    }
    if (typeof detail === 'string' && detail.length > 0) {
      status.textContent = detail;
    }
  }

  // Initial stage — Rust emits "spawn" right before spawning the sidecar but
  // we may load the splash before that fires, so seed a baseline here.
  applyStage(8, '正在启动…');

  const tauri = window.__TAURI__;
  if (!tauri || !tauri.event) {
    // Dev / fallback: keep the indeterminate shimmer + elapsed counter, drop
    // the timed status reassurance to avoid lying about progress.
    return;
  }
  tauri.event.listen('splash://stage', function (e) {
    if (e && e.payload) applyStage(e.payload.percent, e.payload.detail);
  });
})();
