// Splash auxiliary script. Rust side (lib.rs) is responsible for actually
// navigating to the Control UI once the gateway sidecar reports ready. This
// file just shows a graceful timeout message if that never happens.
(function () {
  const status = document.getElementById('status');
  const error = document.getElementById('error');
  const startedAt = Date.now();
  setInterval(function () {
    const secs = Math.floor((Date.now() - startedAt) / 1000);
    if (secs < 5) return;
    if (secs < 300) {
      status.textContent = '正在启动 OpenClaw Gateway… (' + secs + 's)';
    } else {
      status.hidden = true;
      error.hidden = false;
      error.textContent = '5 分钟内 Gateway 没有启动。可能原因:\n  - 端口 18789 被占用\n  - 内嵌的 Node.js 或 OpenClaw 资源解压失败\n请联系开发者或查看 %LOCALAPPDATA%/Temp/openclaw/openclaw-*.log';
    }
  }, 1000);
})();
