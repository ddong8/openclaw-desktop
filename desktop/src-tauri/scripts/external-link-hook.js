// Injected before every page load. Forwards external link clicks,
// window.open calls, and file:// references to the system default app
// via tauri-plugin-opener.
(function () {
  if (window.__OPENCLAW_OPENER_HOOK__) return;
  window.__OPENCLAW_OPENER_HOOK__ = true;

  function isInternalLoopback(u) {
    if (!u) return false;
    const proto = u.protocol;
    if (proto === 'tauri:' || proto === 'about:' || proto === 'data:' || proto === 'blob:') return true;
    if (proto === 'http:' || proto === 'https:' || proto === 'ws:' || proto === 'wss:') {
      return u.hostname === '127.0.0.1' || u.hostname === 'localhost';
    }
    return false;
  }

  function parseUrl(raw) {
    try { return new URL(raw, window.location.href); } catch { return null; }
  }

  function callOpener(url) {
    try {
      const g = window.__TAURI__;
      if (g && g.opener && typeof g.opener.openUrl === 'function') {
        g.opener.openUrl(url);
        return true;
      }
    } catch {}
    try {
      const invoke =
        (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) ||
        (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
      if (typeof invoke === 'function') {
        invoke('plugin:opener|open_url', { url });
        return true;
      }
    } catch (e) {
      console.warn('[openclaw-desktop] opener invoke failed:', e);
    }
    return false;
  }

  function handleExternal(raw) {
    const u = parseUrl(raw);
    if (!u) return false;
    if (isInternalLoopback(u)) return false;
    return callOpener(u.toString());
  }

  // Click capture on every anchor / element with [data-href]
  document.addEventListener('click', function (e) {
    let el = e.target;
    while (el && el !== document) {
      if (el.tagName === 'A') {
        const href = el.getAttribute('href');
        if (href && (el.target === '_blank' || handleExternal(href))) {
          if (handleExternal(href)) {
            e.preventDefault();
            e.stopPropagation();
            return;
          }
        }
      }
      el = el.parentNode || el.host || null;
    }
  }, true);

  // Aux interception: middle-click / Ctrl+click also open new tabs in normal browsers
  document.addEventListener('auxclick', function (e) {
    if (e.button !== 1) return;
    let el = e.target;
    while (el && el !== document) {
      if (el.tagName === 'A' && el.href && handleExternal(el.href)) {
        e.preventDefault();
        e.stopPropagation();
        return;
      }
      el = el.parentNode || el.host || null;
    }
  }, true);

  // window.open shim
  const origOpen = window.open;
  window.open = function (url, target, features) {
    if (url) {
      const u = parseUrl(url);
      if (u && !isInternalLoopback(u) && callOpener(u.toString())) {
        return null;
      }
    }
    return origOpen ? origOpen.call(window, url, target, features) : null;
  };
})();
