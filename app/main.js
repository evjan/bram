// Tauri 2 exposes the API on window.__TAURI__ when withGlobalTauri is true.
// https://v2.tauri.app/reference/javascript/api/
const { invoke, Channel } = window.__TAURI__.core;

invoke("log_from_right_pane", {
  payload: { kind: "main.js-loaded", at: new Date().toISOString() },
}).catch(() => {});

const TERM_FONT_KEY = "bram.terminal.fontSize";
const LEGACY_TERM_FONT_KEY = "xmlui-desktop.terminal.fontSize";
const TERM_FONT_MIN = 8;
const TERM_FONT_MAX = 32;
const TERM_FONT_DEFAULT = 13;

const clampFontSize = (n) =>
  Math.max(TERM_FONT_MIN, Math.min(TERM_FONT_MAX, Math.round(Number(n) || 0)));

const readSavedFontSize = () => {
  try {
    const raw = parseInt(
      localStorage.getItem(TERM_FONT_KEY) ??
        localStorage.getItem(LEGACY_TERM_FONT_KEY) ??
        "",
      10,
    );
    return Number.isFinite(raw) ? clampFontSize(raw) : TERM_FONT_DEFAULT;
  } catch {
    return TERM_FONT_DEFAULT;
  }
};

const term = new Terminal({
  fontFamily: 'Menlo, Monaco, "Courier New", monospace',
  fontSize: readSavedFontSize(),
  cursorBlink: true,
  theme: { background: "#000000", foreground: "#e0e0e0" },
  scrollback: 10000,
  allowProposedApi: true,
});

const fitAddon = new FitAddon.FitAddon();
term.loadAddon(fitAddon);
const PTY_RESIZE_MIN_INTERVAL_MS = 40;
const VIEWPORT_RESTORE_WINDOW_MS = 750;
const isWindows = navigator.userAgent.toLowerCase().includes("windows");

const container = document.getElementById("terminal");
term.open(container);
window.submitTerminalEnter = function () {
  try {
    term.focus();
    invoke("pty_write", { data: "\r" }).catch((e) =>
      console.error("submitTerminalEnter pty_write", e),
    );
  } catch (e) {
    console.error("submitTerminalEnter", e);
  }
};

window.submitTerminalTurn = function (text) {
  try {
    term.focus();
    invoke("pty_write", { data: String(text) })
      .then(() => invoke("pty_write", { data: "\r" }))
      .catch((e) => console.error("submitTerminalTurn pty_write", e));
  } catch (e) {
    console.error("submitTerminalTurn", e);
  }
};

try {
  const webgl = new WebglAddon.WebglAddon();
  term.loadAddon(webgl);
  webgl.onContextLoss(() => webgl.dispose());
} catch (e) {
  console.warn("webgl addon failed, falling back to canvas/dom renderer", e);
}

const captureViewport = () => {
  const buffer = term.buffer?.active;
  if (!buffer) return null;
  const viewportEl = container.querySelector(".xterm-viewport");
  return {
    viewportY: buffer.viewportY || 0,
    baseY: buffer.baseY || 0,
    atBottom: (buffer.baseY || 0) - (buffer.viewportY || 0) <= 1,
    domScrollTop: viewportEl ? viewportEl.scrollTop : null,
  };
};

const restoreViewport = (snapshot) => {
  if (!snapshot) return;
  const buffer = term.buffer?.active;
  if (!buffer) return;
  const viewportEl = container.querySelector(".xterm-viewport");
  if (snapshot.atBottom) {
    term.scrollToBottom();
    if (viewportEl) viewportEl.scrollTop = viewportEl.scrollHeight;
    return;
  }
  const maxViewport = buffer.baseY || 0;
  const target = Math.max(0, Math.min(snapshot.viewportY, maxViewport));
  term.scrollToLine(target);
  if (viewportEl && snapshot.domScrollTop !== null) {
    viewportEl.scrollTop = snapshot.domScrollTop;
  }
};

let pendingViewportRestore = null;
let pendingViewportRestoreUntil = 0;
let pendingViewportRestoreTimer = null;

const clearPendingViewportRestore = () => {
  pendingViewportRestore = null;
  pendingViewportRestoreUntil = 0;
  clearTimeout(pendingViewportRestoreTimer);
  pendingViewportRestoreTimer = null;
};

const armViewportRestore = (snapshot) => {
  if (!snapshot) return;
  pendingViewportRestore = snapshot;
  pendingViewportRestoreUntil = Date.now() + VIEWPORT_RESTORE_WINDOW_MS;
  clearTimeout(pendingViewportRestoreTimer);
  pendingViewportRestoreTimer = setTimeout(
    clearPendingViewportRestore,
    VIEWPORT_RESTORE_WINDOW_MS,
  );
};

const restorePendingViewport = () => {
  if (!pendingViewportRestore) return;
  if (Date.now() > pendingViewportRestoreUntil) {
    clearPendingViewportRestore();
    return;
  }
  restoreViewport(pendingViewportRestore);
};

const runTerminalFit = ({ preserveViewport = true } = {}) => {
  const snapshot = preserveViewport ? captureViewport() : null;
  fitAddon.fit();
  if (!snapshot) return;
  armViewportRestore(snapshot);
  if (!resizing) {
    requestAnimationFrame(() => {
      restorePendingViewport();
      requestAnimationFrame(() => restorePendingViewport());
    });
  }
};

const scheduleStartupTerminalFit = () => {
  const run = () => {
    scheduleTerminalFit({ preserveViewport: false });
    if (isWindows) {
      setTimeout(() => scheduleTerminalFit({ preserveViewport: false }), 150);
      setTimeout(() => scheduleTerminalFit({ preserveViewport: false }), 500);
    }
  };
  const afterReady = () => {
    requestAnimationFrame(() => {
      requestAnimationFrame(run);
    });
  };
  const fontsReady = document.fonts?.ready;
  if (fontsReady && typeof fontsReady.then === "function") {
    fontsReady.then(afterReady, afterReady);
  } else {
    afterReady();
  }
};

let fitScheduled = false;
let fitNeedsViewportPreserve = false;
const scheduleTerminalFit = ({ preserveViewport = true } = {}) => {
  fitNeedsViewportPreserve = fitNeedsViewportPreserve || preserveViewport;
  if (fitScheduled) return;
  fitScheduled = true;
  requestAnimationFrame(() => {
    const shouldPreserve = fitNeedsViewportPreserve;
    fitScheduled = false;
    fitNeedsViewportPreserve = false;
    runTerminalFit({ preserveViewport: shouldPreserve });
  });
};

let terminalResizeObserver = null;
let terminalResizeObserverTimer = null;
let lastObservedTerminalSize = null;
const observeTerminalSize = () => {
  if (!window.ResizeObserver || !container) return;
  terminalResizeObserver = new ResizeObserver((entries) => {
    const entry = entries[0];
    if (!entry) return;
    const { width, height } = entry.contentRect;
    if (width <= 0 || height <= 0) return;
    const size = `${Math.round(width)}x${Math.round(height)}`;
    if (size === lastObservedTerminalSize) return;
    lastObservedTerminalSize = size;
    clearTimeout(terminalResizeObserverTimer);
    terminalResizeObserverTimer = setTimeout(() => {
      scheduleTerminalFit({ preserveViewport: false });
    }, 0);
  });
  terminalResizeObserver.observe(container);
};

observeTerminalSize();

let resizing = false;
let resizingRestoreTimer = null;
window.addEventListener("resize", () => {
  resizing = true;
  clearTimeout(resizingRestoreTimer);
  resizingRestoreTimer = setTimeout(() => {
    resizing = false;
    requestAnimationFrame(() => {
      restorePendingViewport();
      requestAnimationFrame(() => restorePendingViewport());
    });
  }, 50);
  scheduleTerminalFit();
});

const setTerminalFontSize = (n) => {
  const size = clampFontSize(n);
  term.options.fontSize = size;
  runTerminalFit();
  try {
    localStorage.setItem(TERM_FONT_KEY, String(size));
  } catch {}
};

const isMac = /Mac|iPhone|iPod|iPad/i.test(navigator.userAgent);

const isTerminalEventTarget = (target) => {
  if (!target || !container) return false;
  return target === container || container.contains(target);
};

const writeTerminalPaste = (text, source) => {
  if (!text) return;
  invoke("pty_write", {
    data: "\x1b[200~" + text + "\x1b[201~",
  }).catch((e) => console.error("pty_write paste", source, e));
};

const copyTerminalSelection = (text) => {
  if (!text) return;
  if (navigator.clipboard?.writeText) {
    navigator.clipboard.writeText(text).catch((e) => {
      console.error("clipboard write", e);
      copyTerminalSelectionFallback(text);
    });
    return;
  }
  copyTerminalSelectionFallback(text);
};

const copyTerminalSelectionFallback = (text) => {
  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "readonly");
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  document.body.appendChild(textarea);
  textarea.select();
  try {
    document.execCommand("copy");
  } catch (e) {
    console.error("clipboard copy fallback", e);
  } finally {
    textarea.remove();
    term.focus();
  }
};

const readClipboardAndPasteToTerminal = (source) => {
  if (!navigator.clipboard?.readText) return false;
  navigator.clipboard
    .readText()
    .then((text) => writeTerminalPaste(text, source))
    .catch((e) => console.error("clipboard read", source, e));
  return true;
};

container.addEventListener("paste", (ev) => {
  if (isMac || !isTerminalEventTarget(ev.target)) return;
  const text = ev.clipboardData?.getData("text/plain");
  if (!text) return;
  ev.preventDefault();
  writeTerminalPaste(text, "paste-event");
});

term.attachCustomKeyEventHandler((ev) => {
  if (ev.type !== "keydown") return true;
  // Don't interfere with AltGr (Ctrl+Alt on Win/Linux produces @, |, [, ]
  // etc. on non-US layouts).
  if (ev.altKey) return true;

  // Non-macOS terminal copy/paste:
  //   - Plain Ctrl+C: copies the selection when one exists (Windows Terminal
  //     behavior); falls through to xterm.js → SIGINT when there's no
  //     selection. We avoid Ctrl+Shift+C because WebView2 owns that combo
  //     at the native layer for the Edge "Inspect Element" devtools
  //     accelerator, which fires before our JS handler can preventDefault.
  //   - Plain Ctrl+V and Ctrl+Shift+V paste clipboard text via the
  //     bracketed-paste path. Ctrl+V is the Windows expectation; Ctrl+Shift+V
  //     remains supported for Linux terminal muscle memory.
  //     preventDefault stops the WebView's native paste event from also
  //     firing — without it, xterm.js's textarea paste listener would
  //     write the clipboard a second time.
  if (!isMac && ev.ctrlKey && !ev.shiftKey && (ev.key === "c" || ev.key === "C")) {
    const sel = term.getSelection();
    if (sel) {
      ev.preventDefault();
      copyTerminalSelection(sel);
      return false;
    }
    // No selection: let xterm.js send ^C → SIGINT.
  }
  if (!isMac && ev.ctrlKey && (ev.key === "V" || ev.key === "v")) {
    const handled = readClipboardAndPasteToTerminal(
      ev.shiftKey ? "ctrl-shift-v" : "ctrl-v",
    );
    if (!handled) return true;
    ev.preventDefault();
    return false;
  }

  // Font-size shortcuts: Cmd on macOS, Ctrl elsewhere.
  const mod = isMac ? ev.metaKey : ev.ctrlKey;
  if (!mod) return true;
  if (ev.key === "=" || ev.key === "+") {
    setTerminalFontSize(term.options.fontSize + 1);
    return false;
  }
  if (ev.key === "-" || ev.key === "_") {
    setTerminalFontSize(term.options.fontSize - 1);
    return false;
  }
  if (ev.key === "0") {
    setTerminalFontSize(TERM_FONT_DEFAULT);
    return false;
  }
  return true;
});

document
  .getElementById("font-smaller")
  ?.addEventListener("click", () => setTerminalFontSize(term.options.fontSize - 1));
document
  .getElementById("font-larger")
  ?.addEventListener("click", () => setTerminalFontSize(term.options.fontSize + 1));

(() => {
  const TERMINAL_HIDDEN_KEY = "bram.terminal.hidden";
  const LEGACY_TERMINAL_HIDDEN_KEY = "xmlui-desktop.terminal.hidden";
  const btn = document.getElementById("toggle-terminal");
  if (!btn) return;

  const apply = (hidden) => {
    document.body.classList.toggle("terminal-hidden", hidden);
    if (!hidden) {
      // Re-measure xterm.js once the layout settles.
      scheduleTerminalFit();
    }
  };

  let initial = false;
  try {
    initial =
      (localStorage.getItem(TERMINAL_HIDDEN_KEY) ??
        localStorage.getItem(LEGACY_TERMINAL_HIDDEN_KEY)) === "1";
  } catch {}
  apply(initial);
  if (!initial) {
    scheduleStartupTerminalFit();
  }

  btn.addEventListener("click", () => {
    const hidden = !document.body.classList.contains("terminal-hidden");
    apply(hidden);
    try {
      localStorage.setItem(TERMINAL_HIDDEN_KEY, hidden ? "1" : "0");
    } catch {}
  });
})();

// Vertical splitter between terminal (left pane) and right column.
// Persists the chosen flexBasis pixel width to localStorage as
// `bram.splitter.left` and rehydrates it on startup. Pixels (not
// percentage) match how the drag handler writes flexBasis.
const LEFT_SPLITTER_KEY = "bram.splitter.left";
(() => {
  const left = document.querySelector(".pane-left");
  if (!left) return;
  const raw = localStorage.getItem(LEFT_SPLITTER_KEY);
  const px = parseFloat(raw);
  if (!isNaN(px) && px > 0) {
    left.style.flexBasis = px + "px";
  }
})();
(() => {
  const splitter = document.getElementById("splitter");
  const left = document.querySelector(".pane-left");
  const split = document.querySelector(".split");
  if (!splitter || !left || !split) return;

  const MIN_PX = 200;

  splitter.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    splitter.setPointerCapture(e.pointerId);
    splitter.classList.add("dragging");
    document.body.classList.add("splitter-dragging");

    let lastX = null;
    const onMove = (ev) => {
      const rect = split.getBoundingClientRect();
      let x = ev.clientX - rect.left;
      const max = rect.width - MIN_PX - splitter.offsetWidth;
      if (x < MIN_PX) x = MIN_PX;
      if (x > max) x = max;
      lastX = x;
      left.style.flexBasis = x + "px";
      scheduleTerminalFit();
    };
    const onUp = (ev) => {
      splitter.releasePointerCapture(ev.pointerId);
      splitter.classList.remove("dragging");
      document.body.classList.remove("splitter-dragging");
      splitter.removeEventListener("pointermove", onMove);
      splitter.removeEventListener("pointerup", onUp);
      if (lastX !== null && lastX > 0) {
        const val = String(Math.round(lastX));
        localStorage.setItem(LEFT_SPLITTER_KEY, val);
        console.log("[splitter-save]", LEFT_SPLITTER_KEY, val);
      }
      runTerminalFit();
    };
    splitter.addEventListener("pointermove", onMove);
    splitter.addEventListener("pointerup", onUp);
  });
})();

// Horizontal splitter resizes the tools drawer (only operative when drawer
// is open; the splitter is `display: none` when the .hidden class is set).
// Persists the chosen flexBasis percentage to localStorage as
// `bram.splitter.shell` and rehydrates it on startup.
const SHELL_SPLITTER_KEY = "bram.splitter.shell";
const TOOLS_ROUTE_KEY = "bram.tools.route";
const LEGACY_TOOLS_ROUTE_KEY = "xmlui-desktop.tools.route";
const TOOLS_ROUTE_POLL_MS = 500;
const TOOLS_HIDDEN_KEY = "bram.tools.hidden";

const logShellEvent = (payload) => {
  try {
    invoke("log_from_right_pane", { payload }).catch(() => {});
  } catch {}
};

(() => {
  const tools = document.getElementById("tools-pane");
  if (!tools) return;
  const raw = localStorage.getItem(SHELL_SPLITTER_KEY);
  const pct = parseFloat(raw);
  if (!isNaN(pct) && pct > 0 && pct < 100) {
    tools.style.flexBasis = pct + "%";
  }
})();
(() => {
  const hSplitter = document.getElementById("h-splitter");
  const column = document.querySelector(".right-column");
  if (!hSplitter || !column) return;

  const MIN_PX = 80;

  hSplitter.addEventListener("pointerdown", (e) => {
    // Re-query the tools iframe on each drag — swapToolsIframe replaces
    // it on every watcher reload, and a captured reference from IIFE
    // boot would point at a detached node (cursor changed on hover but
    // dragging silently set flexBasis on a node no longer in the layout).
    const tools = document.getElementById("tools-pane");
    if (!tools) return;
    e.preventDefault();
    hSplitter.setPointerCapture(e.pointerId);
    hSplitter.classList.add("dragging");
    document.body.classList.add("splitter-dragging");

    let lastPct = null;
    const onMove = (ev) => {
      const rect = column.getBoundingClientRect();
      // Drawer height = distance from pointer to bottom of column.
      let h = rect.bottom - ev.clientY;
      const max = rect.height - MIN_PX - hSplitter.offsetHeight;
      if (h < MIN_PX) h = MIN_PX;
      if (h > max) h = max;
      // Store as percentage of the column height so window resizes
      // preserve the user's chosen proportion. Absolute pixels (h +
      // "px") locked the drawer to a fixed height and left a gap or
      // overshoot when the Tauri window grew or shrank.
      lastPct = (h / rect.height) * 100;
      tools.style.flexBasis = lastPct + "%";
    };
    const onUp = (ev) => {
      hSplitter.releasePointerCapture(ev.pointerId);
      hSplitter.classList.remove("dragging");
      document.body.classList.remove("splitter-dragging");
      hSplitter.removeEventListener("pointermove", onMove);
      hSplitter.removeEventListener("pointerup", onUp);
      if (lastPct !== null && lastPct > 0 && lastPct < 100) {
        const val = String(Math.round(lastPct * 10) / 10);
        localStorage.setItem(SHELL_SPLITTER_KEY, val);
        console.log("[splitter-save]", SHELL_SPLITTER_KEY, val);
      }
    };
    hSplitter.addEventListener("pointermove", onMove);
    hSplitter.addEventListener("pointerup", onUp);
  });
})();

// Bottom-toolbar drawer toggle. v1 has a single "tools" button — later
// stages will add per-tool buttons (Workspace, Sessions) that swap the
// tools iframe's hash route while keeping the drawer open.
(() => {
  const btn = document.getElementById("toggle-tools");
  const tools = document.getElementById("tools-pane");
  const hSplitter = document.getElementById("h-splitter");
  if (!btn || !tools || !hSplitter) return;
  try {
    const hidden = localStorage.getItem(TOOLS_HIDDEN_KEY) === "1";
    tools.classList.toggle("hidden", hidden);
    hSplitter.classList.toggle("hidden", hidden);
    logShellEvent({
      kind: "tools-drawer-restore",
      hidden,
      at: new Date().toISOString(),
    });
  } catch {}
  btn.addEventListener("click", () => {
    const opening = tools.classList.contains("hidden");
    const hidden = !opening;
    tools.classList.toggle("hidden", hidden);
    hSplitter.classList.toggle("hidden", hidden);
    try {
      localStorage.setItem(TOOLS_HIDDEN_KEY, hidden ? "1" : "0");
    } catch {}
    logShellEvent({
      kind: "tools-drawer-save",
      hidden,
      at: new Date().toISOString(),
    });
  });
})();

// PTY wiring: stdout from Rust arrives over a Channel; stdin goes via invoke.
// https://v2.tauri.app/develop/calling-frontend/#channels
const ptyChannel = new Channel();

let startupFitDone = false;
ptyChannel.onmessage = (chunk) => {
  term.write(new Uint8Array(chunk));
  if (!startupFitDone) {
    startupFitDone = true;
    // First PTY output means the shell has started and the WebView2 window
    // has finished its initial layout pass — safe to do a definitive fit.
    requestAnimationFrame(() => scheduleTerminalFit({ preserveViewport: false }));
  }
  if (pendingViewportRestore) {
    requestAnimationFrame(() => {
      restorePendingViewport();
      requestAnimationFrame(() => restorePendingViewport());
    });
  }
};

term.onData((data) => {
  invoke("pty_write", { data }).catch((e) => console.error("pty_write", e));
});

let pendingPtySize = null;
let lastSentPtySize = null;
let ptyResizeTimer = null;
let lastPtyResizeAt = 0;

const samePtySize = (a, b) => !!a && !!b && a.cols === b.cols && a.rows === b.rows;

const flushPtyResize = () => {
  if (!pendingPtySize) return;
  const now = Date.now();
  const sinceLast = now - lastPtyResizeAt;
  if (sinceLast < PTY_RESIZE_MIN_INTERVAL_MS) {
    clearTimeout(ptyResizeTimer);
    ptyResizeTimer = setTimeout(
      flushPtyResize,
      PTY_RESIZE_MIN_INTERVAL_MS - sinceLast,
    );
    return;
  }
  const next = pendingPtySize;
  pendingPtySize = null;
  if (samePtySize(next, lastSentPtySize)) return;
  armViewportRestore(captureViewport());
  lastSentPtySize = next;
  lastPtyResizeAt = now;
  invoke("pty_resize", next).catch((e) => console.error("pty_resize", e));
};

term.onResize(({ cols, rows }) => {
  const next = { cols, rows };
  if (samePtySize(next, pendingPtySize) || samePtySize(next, lastSentPtySize)) return;
  pendingPtySize = next;
  flushPtyResize();
});
const ptyShell = isWindows
  ? {
      cmd: "powershell.exe",
      args: [
        "-NoLogo",
        "-NoExit",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        "./app/shell/claude-code-profile.ps1",
      ],
    }
  : {
      cmd: "/bin/bash",
      args: ["--noprofile", "--rcfile", "./app/shell/claude-code-shellrc", "-i"],
    };

// Delay pty_spawn until fonts and layout have settled so the PTY gets
// the correct initial dimensions. If we spawn immediately, term.cols/rows
// are xterm.js defaults (80×24); the subsequent fitAddon.fit() then sends
// pty_resize, but shells don't redraw existing output after SIGWINCH so
// the prompt stays confined to the upper portion of the terminal.
const _spawnPty = async () => {
  // Fit right before reading cols/rows so the PTY inherits the actual
  // container dimensions, not the xterm.js defaults.
  fitAddon.fit();
  try {
    await invoke("pty_spawn", {
      ...ptyShell,
      cols: term.cols,
      rows: term.rows,
      onData: ptyChannel,
    });
    term.focus();
  } catch (e) {
    term.writeln(`\r\n\x1b[31mfailed to start pty: ${e}\x1b[0m`);
  }
};
{
  const _ready = document.fonts?.ready;
  const _afterFonts = () =>
    requestAnimationFrame(() => requestAnimationFrame(_spawnPty));
  if (_ready && typeof _ready.then === "function") {
    _ready.then(_afterFonts, _afterFonts);
  } else {
    _afterFonts();
  }
}

// Right-pane base URL is provisioned by the Rust backend on startup —
// it returns the tauri:// scheme URL whose path the scheme handler
// routes to the project's content (`/__project/*` proxied to the
// loopback HTTP server) or to embedded shell assets. We ask for it
// before setting iframe.src so the path picked by the backend wins
// over any default; reload happens via re-assigning src with a cache
// buster.
const { listen } = window.__TAURI__.event;
(async () => {
  const iframe = document.getElementById("right-pane");
  if (!iframe) return;
  let RIGHT_PANE_SRC, TOOLS_PANE_SRC;
  try {
    [RIGHT_PANE_SRC, TOOLS_PANE_SRC] = await Promise.all([
      invoke("get_right_pane_url"),
      invoke("get_tools_pane_url"),
    ]);
  } catch (e) {
    console.error("get_*_pane_url failed", e);
    return;
  }
  const tools = document.getElementById("tools-pane");
  // Cache-bust by appending t=<now>. RIGHT_PANE_SRC may already contain
  // a path (e.g. http://localhost:8080/) but no query string — we
  // document `path` in .bram.json as path-only, so `?` is safe.
  const bust = (u) => u + (u.includes("?") ? "&" : "?") + "t=" + Date.now();
  const normalizeToolsRoute = (loc) => {
    const hash = loc?.hash || "";
    if (hash.startsWith("#/")) return hash;
    const path = loc?.pathname || "";
    if (!path || path === "/" || path === "/tools/" || path === "/tools/index.html") {
      return "";
    }
    return path.startsWith("/") ? "#" + path : "#/" + path;
  };
  const readSavedToolsHash = () => {
    try {
      const saved =
        localStorage.getItem(TOOLS_ROUTE_KEY) ||
        localStorage.getItem(LEGACY_TOOLS_ROUTE_KEY) ||
        "";
      const route = saved.startsWith("#/") ? saved : "";
      logShellEvent({
        kind: "tools-route-read",
        saved,
        route,
        at: new Date().toISOString(),
      });
      return route;
    } catch {
      return "";
    }
  };
  let lastPersistedToolsRoute = "";
  const persistToolsRoute = (toolsWindow) => {
    try {
      const route = normalizeToolsRoute(toolsWindow?.location);
      if (route && route !== lastPersistedToolsRoute) {
        localStorage.setItem(TOOLS_ROUTE_KEY, route);
        lastPersistedToolsRoute = route;
        logShellEvent({
          kind: "tools-route-save",
          route,
          pathname: toolsWindow?.location?.pathname || "",
          hash: toolsWindow?.location?.hash || "",
          at: new Date().toISOString(),
        });
      }
      return route;
    } catch {
      return "";
    }
  };
  const toolsSrcWithHash = (src, hash) => {
    const route = hash || readSavedToolsHash();
    logShellEvent({
      kind: "tools-route-restore",
      route,
      hasHash: !!hash,
      at: new Date().toISOString(),
    });
    return route ? src + route : src;
  };
  // Double-buffer swap for the tools iframe so reloads don't flash a
  // blank frame. Create a new iframe off-screen, wait for `load`, then
  // promote it (replace the old in the DOM with the new, inheriting the
  // id/class/style so the rest of the parent shell keeps working). A
  // single-flight guard prevents overlapping swaps; the debounced
  // watcher emits at most one reload per 500ms anyway.
  let toolsSwapping = false;
  function swapToolsIframe(newSrc) {
    const oldTools = document.getElementById("tools-pane");
    if (!oldTools) return;
    const parent = oldTools.parentElement;
    if (!parent || toolsSwapping) return;
    toolsSwapping = true;

    // Preserve the current XMLUI route across hot-reload. Without this,
    // the new iframe loads tools/index.html with no hash → router
    // restarts at the root route, yanking the user away from
    // /worklist or wherever else they were. Same-origin iframe
    // (tauri://localhost), so contentWindow.location.hash is readable.
    let preservedHash = "";
    try {
      preservedHash = persistToolsRoute(oldTools.contentWindow);
    } catch (e) {}

    const newTools = document.createElement("iframe");
    newTools.setAttribute("allow", oldTools.getAttribute("allow") || "");
    // Load off-screen so the user never sees the blank intermediate
    // state. Tiny size keeps it from affecting layout.
    newTools.style.cssText =
      "position:absolute;visibility:hidden;left:-99999px;top:0;width:1px;height:1px;";

    function onLoad() {
      newTools.removeEventListener("load", onLoad);
      // Promote: inherit the live class/style and the id, then replace
      // in the same DOM position. The toggle-tools button keeps working
      // because it queries by id on each click.
      newTools.className = oldTools.className;
      newTools.style.cssText = oldTools.style.cssText;
      newTools.id = "tools-pane";
      parent.replaceChild(newTools, oldTools);
      toolsSwapping = false;
    }
    newTools.addEventListener("load", onLoad);
    parent.appendChild(newTools);
    newTools.src = toolsSrcWithHash(newSrc, preservedHash);
  }
  // reloadAll: reload BOTH iframes. Used by the manual "reload xmlui app"
  // toolbar button and by the "tools-pane-reload" watcher event (drawer
  // code changed, both panes may consume it). Right pane swaps src in
  // place (the user's project app handles its own loading state); tools
  // pane goes through swapToolsIframe to avoid the flash.
  function reloadAll() {
    iframe.src = bust(RIGHT_PANE_SRC);
    swapToolsIframe(bust(TOOLS_PANE_SRC));
  }
  // reloadRightPaneOnly: reload only the right pane. Used by the
  // "right-pane-reload" watcher event for user-project file changes AND
  // for .bram.json hot-reload (path/query updates). We re-fetch
  // the URL each time instead of reusing the captured one so config edits
  // are picked up. The drawer is poll-driven so it does NOT need to reload
  // here, and keeping it stable avoids postMessage-vs-iframe-rebuild races
  // on Approve/Drop clicks while the agent is writing files.
  async function reloadRightPaneOnly() {
    try {
      RIGHT_PANE_SRC = await invoke("get_right_pane_url");
    } catch (e) {
      console.error("get_right_pane_url failed", e);
    }
    iframe.src = bust(RIGHT_PANE_SRC);
  }
  // Single-shot retry: if the right-pane iframe hasn't fired `load`
  // within 1.5s, the project-managed server (from .bram.json)
  // is probably still starting up — connection is stuck. Bust and try
  // once more. Iframes fire `load` even for error pages, so this
  // specifically catches the "still connecting" state. `error` is not
  // reliable for iframes; we don't bother listening for it.
  let loaded = false;
  iframe.addEventListener("load", () => { loaded = true; });
  iframe.src = RIGHT_PANE_SRC;
  setTimeout(() => {
    if (!loaded) iframe.src = bust(RIGHT_PANE_SRC);
  }, 1500);
  if (tools) tools.src = toolsSrcWithHash(TOOLS_PANE_SRC);
  setInterval(() => {
    const currentTools = document.getElementById("tools-pane");
    if (currentTools?.contentWindow) persistToolsRoute(currentTools.contentWindow);
  }, TOOLS_ROUTE_POLL_MS);
  document
    .getElementById("reload-right")
    ?.addEventListener("click", reloadAll);
  document
    .getElementById("open-devtools")
    ?.addEventListener("click", () => {
      invoke("open_devtools").catch((e) => console.error("open_devtools", e));
    });
  listen("right-pane-reload", reloadRightPaneOnly);
  listen("tools-pane-reload", reloadAll);
  // #150 follow-up (refs #170): the host now stores right-pane-reload
  // and tools-pane-reload payloads via emit_replayable_signal so a
  // late-attaching listener can recover them. main.js's listeners
  // attach after Tauri is ready; if the host fired a reload during
  // the gap (project-config-reload at startup, etc.), the live emit
  // was lost and the iframes stayed frozen. Ask the host to replay
  // each on attach; the request is idempotent on no-stored-payload.
  fetch("/__startup-ready?event=right-pane-reload", { cache: "no-store" }).catch(() => {});
  fetch("/__startup-ready?event=tools-pane-reload", { cache: "no-store" }).catch(() => {});
})();

// ui.targetAppMinimized driver. The Settings tab and hand-edits to
// .bram.json both flow here via the `settings-changed` event (emitted
// by handle_project_config_reload in src-tauri/src/lib.rs). True =
// drive the right-column h-splitter to its MIN_PX floor on the
// target-app side, leaving the tools drawer to fill the column. False
// = clear the override and let the splitter fall back to default flex
// behavior; user drags are not persisted, so prior manual sizes are
// not restored.
(() => {
  const TARGET_MIN_PX = 80; // matches the h-splitter MIN_PX above
  let minimized = false;
  function pin() {
    const tools = document.getElementById("tools-pane");
    const column = document.querySelector(".right-column");
    const hSplitter = document.getElementById("h-splitter");
    if (!tools || !column || !hSplitter) return;
    const rect = column.getBoundingClientRect();
    const h = rect.height - TARGET_MIN_PX - hSplitter.offsetHeight;
    if (h > 0) tools.style.flexBasis = h + "px";
  }
  function set(next) {
    const prev = minimized;
    minimized = !!next;
    if (minimized) {
      pin();
    } else if (prev) {
      // Only clear the override when transitioning OFF, not on every
      // tick of the resize listener — otherwise a non-minimized user
      // drag (stored as percentage by the h-splitter handler) gets
      // wiped on every window resize.
      const tools = document.getElementById("tools-pane");
      if (tools) tools.style.flexBasis = "";
    }
  }
  fetch("/__settings")
    .then((r) => r.json())
    .then((v) => set(v && v.ui && v.ui.targetAppMinimized))
    .catch(() => {});
  listen("settings-changed", (e) => {
    const v = e && e.payload;
    set(v && v.ui && v.ui.targetAppMinimized);
  });
  // Reapply on window resize so the splitter stays at the floor when
  // the Tauri window grows or shrinks. No-op when not minimized — the
  // user's drag-set percentage flexBasis is responsive on its own.
  window.addEventListener("resize", () => {
    if (minimized) pin();
  });
})();

// Click-to-toggle voice. The toolbar 🎤 button toggles its own recording;
// iframes (Workspace, etc.) drive the same recorder via voice-start/voice-stop
// messages. Auto-starts whisper-server on first record click.
(() => {
  const WHISPER_HOST = "http://127.0.0.1:18080";
  const WHISPER_URL = WHISPER_HOST + "/inference";
  // Host-native macOS/Linux launch expands this on the host. Windows launch
  // sends it to WSL so it resolves under the WSL user's home directory.
  const MODEL_PATH = "~/.local/share/whisper-models/ggml-small.en.bin";
  const READY_TIMEOUT_MS = 15000;
  const READY_POLL_MS = 300;

  const toolbarBtn = document.getElementById("voice-toggle");
  if (!toolbarBtn) return;

  // Structured voice-pipeline logging via the existing log_from_right_pane
  // command, so every stage shows up in cargo run stderr tagged with the
  // session's requestId and a timestamp. See the voice-instrumentation
  // worklist item for the rationale.
  const voiceLog = (stage, payload) => {
    try {
      invoke(
        "log_from_right_pane",
        {
          payload: Object.assign(
            { kind: "voice-host", stage, at: new Date().toISOString() },
            payload || {},
          ),
        },
      ).catch(() => {});
    } catch (e) {}
  };
  // Last few transcripts (keyed by requestId) so we can detect when whisper
  // returns a byte-for-byte duplicate of a recent response — the most
  // suspect failure mode behind the "stuck on 'push it'" symptom.
  const recentTranscripts = [];
  const RECENT_TRANSCRIPT_WINDOW_MS = 60_000;

  let mediaRecorder = null;
  let audioChunks = [];
  let stream = null;
  // active === null         → idle
  // active === "toolbar"    → toolbar mic; transcript → pty_write
  // active === { source, requestId } → iframe round-trip; transcript → postMessage
  let active = null;
  let activeStopAtMs = null;
  let activeStopReceivedAtMs = null;
  // Synthetic requestId for toolbar sessions — keeps log entries correlated
  // even though the toolbar path never receives an iframe-supplied id.
  let toolbarRequestId = null;
  const currentRequestId = () =>
    active === "toolbar"
      ? toolbarRequestId
      : active && typeof active === "object"
        ? active.requestId
        : null;

  const setToolbarState = (state) => {
    toolbarBtn.dataset.state = state;
    toolbarBtn.innerHTML =
      state === "recording"
        ? "&#x23F9;"
        : state === "processing"
          ? '<span class="voice-spinner" aria-label="Processing voice input"></span>'
        : state === "starting"
          ? "&#x23F3;"
          : "&#x1F3A4;";
  };
  setToolbarState("idle");

  const mediaRecorderConfig = () => {
    const opusWebm = "audio/webm;codecs=opus";
    if (
      window.MediaRecorder &&
      typeof MediaRecorder.isTypeSupported === "function" &&
      MediaRecorder.isTypeSupported(opusWebm)
    ) {
      return { options: { mimeType: opusWebm }, codecHint: opusWebm };
    }
    return { options: undefined, codecHint: "default" };
  };

  const probeWhisperServer = async (stage) => {
    try {
      const res = await fetch(WHISPER_HOST + "/", {
        method: "GET",
        cache: "no-store",
      });
      voiceLog(stage, { httpStatus: res.status, ready: res.ok });
      return res.ok;
    } catch (e) {
      voiceLog(stage + "-error", { error: String(e) });
      return false;
    }
  };

  const ensureServerRunning = async () => {
    const startedAt = Date.now();
    let polls = 0;
    voiceLog("ensure-server-enter");
    if (await probeWhisperServer("whisper-preflight")) {
      voiceLog("ensure-server-ready", {
        elapsedMs: Date.now() - startedAt,
        source: "preflight",
      });
      return true;
    }
    let status;
    try {
      status = await invoke("whisper_status");
    } catch (e) {
      console.error("whisper_status", e);
      voiceLog("whisper-status-error", { error: String(e) });
      return false;
    }
    voiceLog("whisper-status", {
      running: !!(status && status.running),
      pid: status && status.pid ? status.pid : null,
    });
    voiceLog("ensure-server-status-result", {
      running: !!(status && status.running),
      pid: status && status.pid ? status.pid : null,
    });
    if (status && status.running) {
      voiceLog("ensure-server-ready", {
        elapsedMs: Date.now() - startedAt,
        source: "status",
      });
      return true;
    }
    try {
      voiceLog("ensure-server-start-invoked", { modelPath: MODEL_PATH });
      const pid = await invoke("whisper_start", { modelPath: MODEL_PATH });
      voiceLog("whisper-started", { pid });
    } catch (e) {
      console.error("whisper_start", e);
      voiceLog("whisper-start-error", { error: String(e) });
      voiceLog("ensure-server-start-error", { error: String(e) });
      return false;
    }
    const deadline = Date.now() + READY_TIMEOUT_MS;
    while (Date.now() < deadline) {
      polls += 1;
      try {
        const res = await fetch(WHISPER_HOST + "/", { method: "GET" });
        voiceLog("ensure-server-poll-tick", {
          elapsedMs: Date.now() - startedAt,
          fetchOk: true,
          httpStatus: res.status,
          poll: polls,
        });
        if (res.ok) {
          voiceLog("whisper-ready", { httpStatus: res.status });
          voiceLog("ensure-server-ready", { elapsedMs: Date.now() - startedAt });
          return true;
        }
        voiceLog("whisper-ready-poll", { httpStatus: res.status });
      } catch (e) {
        voiceLog("ensure-server-poll-tick", {
          elapsedMs: Date.now() - startedAt,
          fetchOk: false,
          error: String(e),
          poll: polls,
        });
        voiceLog("whisper-ready-poll-error", { error: String(e) });
      }
      await new Promise((r) => setTimeout(r, READY_POLL_MS));
    }
    voiceLog("whisper-ready-timeout", { timeoutMs: READY_TIMEOUT_MS });
    voiceLog("ensure-server-timeout", {
      elapsedMs: Date.now() - startedAt,
      totalPolls: polls,
    });
    return false;
  };

  const stopStream = () => {
    if (stream) {
      stream.getTracks().forEach((t) => t.stop());
      stream = null;
    }
  };

  const deliverTranscript = (transcript) => {
    const reqId = currentRequestId();
    const text = String(transcript || "");
    const focusTerminalAfterDelivery = active === "toolbar";
    const deliveredAtMs = Date.now();
    const stopToDeliverMs =
      typeof activeStopAtMs === "number" ? deliveredAtMs - activeStopAtMs : null;
    voiceLog("deliverTranscript", {
      requestId: reqId,
      stopAtMs: activeStopAtMs,
      stopToDeliverMs: stopToDeliverMs,
      target:
        active === "toolbar"
          ? "toolbar"
          : active && typeof active === "object"
            ? "iframe"
            : "none",
      transcriptLength: text.length,
      transcriptPreview: text.slice(0, 80),
    });
    if (active && typeof active === "object" && active.source) {
      // iframe round-trip
      try {
        active.source.postMessage(
          {
            type: "voice-into-result",
            requestId: active.requestId,
            transcript: text,
            stopAtMs: activeStopAtMs,
            stopToDeliverMs: stopToDeliverMs,
          },
          "*",
        );
      } catch (e) {
        console.error("postMessage voice-into-result", e);
        voiceLog("deliverTranscript-postMessage-error", {
          requestId: reqId,
          error: String(e),
        });
      }
    } else if (active === "toolbar" && text) {
      // Prefix with "voice: " so the receiving agent (typically Claude Code)
      // can distinguish dictated content from typed input — see the
      // verbal-vs-structured guardrail in app/__shell/conventions.md.
      invoke("pty_write", {
        data: "\x1b[200~voice: " + text + "\x1b[201~\r",
      }).catch((e) => {
        console.error("pty_write voice", e);
        voiceLog("deliverTranscript-pty-error", {
          requestId: reqId,
          error: String(e),
        });
      });
    }
    active = null;
    activeStopAtMs = null;
    activeStopReceivedAtMs = null;
    toolbarRequestId = null;
    if (toolbarBtn.dataset.state !== "idle") setToolbarState("idle");
    if (focusTerminalAfterDelivery) {
      requestAnimationFrame(() => {
        try {
          term.focus();
          voiceLog("deliverTranscript-terminal-focus", { requestId: reqId });
        } catch (e) {
          voiceLog("deliverTranscript-terminal-focus-error", {
            requestId: reqId,
            error: String(e),
          });
        }
      });
    }
  };

  const startRecording = async (target) => {
    const incomingId =
      target === "toolbar"
        ? "toolbar-" + Date.now() + "-" + Math.random().toString(36).slice(2)
        : target && target.requestId;
    voiceLog("startRecording-enter", {
      requestId: incomingId,
      target: target === "toolbar" ? "toolbar" : "iframe",
      activeWas: active === null ? null : active === "toolbar" ? "toolbar" : "iframe",
    });
    if (active) {
      voiceLog("startRecording-rejected-busy", {
        requestId: incomingId,
        activeRequestId: currentRequestId(),
      });
      // Already busy: tell the new requester nothing came of it.
      if (target && typeof target === "object" && target.source) {
        try {
          target.source.postMessage(
            {
              type: "voice-into-result",
              requestId: target.requestId,
              transcript: "",
            },
            "*",
          );
        } catch (_) {}
      }
      return;
    }
    active = target;
    activeStopAtMs = null;
    activeStopReceivedAtMs = null;
    const isToolbar = target === "toolbar";
    if (isToolbar) toolbarRequestId = incomingId;
    if (isToolbar) setToolbarState("starting");
    const ready = await ensureServerRunning();
    if (!ready) {
      console.error("whisper-server did not become ready");
      voiceLog("startRecording-not-ready", { requestId: incomingId });
      const t = active;
      active = null;
      if (isToolbar) setToolbarState("idle");
      if (t && typeof t === "object" && t.source) {
        try {
          t.source.postMessage(
            { type: "voice-into-result", requestId: t.requestId, transcript: "" },
            "*",
          );
        } catch (_) {}
      }
      return;
    }
    try {
      if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
        throw new Error("navigator.mediaDevices.getUserMedia unavailable");
      }
      stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch (e) {
      console.error("getUserMedia", e);
      voiceLog("getUserMedia-error", {
        requestId: incomingId,
        error: String(e),
        name: e && e.name ? e.name : "",
        message: e && e.message ? e.message : "",
      });
      const t = active;
      active = null;
      if (isToolbar) setToolbarState("idle");
      if (t && typeof t === "object" && t.source) {
        try {
          t.source.postMessage(
            { type: "voice-into-result", requestId: t.requestId, transcript: "" },
            "*",
          );
        } catch (_) {}
      }
      return;
    }
    voiceLog("getUserMedia-ok", {
      requestId: incomingId,
      tracks: stream && typeof stream.getAudioTracks === "function"
        ? stream.getAudioTracks().length
        : null,
    });
    audioChunks = [];
    const recorderConfig = mediaRecorderConfig();
    try {
      mediaRecorder = new MediaRecorder(stream, recorderConfig.options);
    } catch (e) {
      console.error("MediaRecorder", e);
      voiceLog("mediaRecorder-create-error", {
        requestId: incomingId,
        error: String(e),
      });
      stopStream();
      const t = active;
      active = null;
      if (isToolbar) setToolbarState("idle");
      if (t && typeof t === "object" && t.source) {
        try {
          t.source.postMessage(
            { type: "voice-into-result", requestId: t.requestId, transcript: "" },
            "*",
          );
        } catch (_) {}
      }
      return;
    }
    mediaRecorder.ondataavailable = (e) => {
      const size = e.data && e.data.size ? e.data.size : 0;
      voiceLog("mediaRecorder-data", {
        requestId: currentRequestId(),
        chunkSize: size,
      });
      if (size > 0) audioChunks.push(e.data);
    };
    mediaRecorder.onstop = async () => {
      const reqId = currentRequestId();
      const onstopAtMs = Date.now();
      stopStream();
      const blobType =
        (mediaRecorder && mediaRecorder.mimeType) ||
        (recorderConfig.options && recorderConfig.options.mimeType) ||
        "audio/webm";
      const blob = new Blob(audioChunks, { type: blobType });
      audioChunks = [];
      voiceLog("mediaRecorder-onstop", {
        requestId: reqId,
        stopAtMs: activeStopAtMs,
        stopToOnstopMs:
          typeof activeStopAtMs === "number" ? onstopAtMs - activeStopAtMs : null,
        blobSize: blob.size,
        mimeType: blob.type,
        mediaRecorderState: mediaRecorder ? mediaRecorder.state : "null",
        codecHint: recorderConfig.codecHint,
      });
      if (blob.size === 0) {
        voiceLog("transcribe-skipped-empty-blob", { requestId: reqId });
        deliverTranscript("");
        return;
      }
      let transcript = "";
      let httpStatus = null;
      try {
        const formData = new FormData();
        formData.append("file", blob, "recording.webm");
        formData.append("response_format", "json");
        const reqStart = Date.now();
        voiceLog("whisper-request", {
          requestId: reqId,
          stopAtMs: activeStopAtMs,
          stopToRequestMs:
            typeof activeStopAtMs === "number" ? reqStart - activeStopAtMs : null,
          blobSize: blob.size,
        });
        const res = await fetch(WHISPER_URL, { method: "POST", body: formData });
        httpStatus = res.status;
        if (res.ok) {
          const data = await res.json();
          transcript = (data.text || "").trim();
        } else {
          console.error("transcribe HTTP", res.status, res.statusText);
        }
        voiceLog("whisper-response", {
          requestId: reqId,
          stopAtMs: activeStopAtMs,
          stopToResponseMs:
            typeof activeStopAtMs === "number" ? Date.now() - activeStopAtMs : null,
          httpStatus: httpStatus,
          elapsedMs: Date.now() - reqStart,
          transcriptLength: transcript.length,
          transcriptPreview: transcript.slice(0, 80),
        });
      } catch (e) {
        console.error("transcribe", e);
        voiceLog("whisper-error", {
          requestId: reqId,
          error: String(e),
          errorName: e && e.name ? e.name : "",
          errorMessage: e && e.message ? e.message : "",
        });
      }
      // Stale-duplicate detection: warn if whisper returned exactly the same
      // text as a recent prior response. This is the prime suspect behind
      // the "different utterance, same wrong transcript" bug.
      if (transcript) {
        const now = Date.now();
        for (let i = recentTranscripts.length - 1; i >= 0; i--) {
          const r = recentTranscripts[i];
          if (now - r.at > RECENT_TRANSCRIPT_WINDOW_MS) {
            recentTranscripts.splice(0, i + 1);
            break;
          }
          if (r.text === transcript) {
            voiceLog("whisper-duplicate-transcript", {
              requestId: reqId,
              previousRequestId: r.requestId,
              ageMs: now - r.at,
              transcriptPreview: transcript.slice(0, 80),
            });
            break;
          }
        }
        recentTranscripts.push({ requestId: reqId, text: transcript, at: now });
      }
      deliverTranscript(transcript);
    };
    mediaRecorder.start();
    voiceLog("mediaRecorder-start", { requestId: incomingId });
    if (isToolbar) {
      setToolbarState("recording");
    } else if (active && typeof active === "object" && active.source) {
      try {
        active.source.postMessage(
          { type: "voice-recording-started", requestId: active.requestId },
          "*",
        );
      } catch (e) {
        voiceLog("voice-recording-started-postmessage-error", {
          requestId: incomingId,
          error: String(e),
        });
      }
    }
  };

  const stopRecording = (stopAtMs, source) => {
    activeStopAtMs = typeof stopAtMs === "number" ? stopAtMs : Date.now();
    activeStopReceivedAtMs = Date.now();
    voiceLog("stopRecording", {
      requestId: currentRequestId(),
      source: source || "unknown",
      stopAtMs: activeStopAtMs,
      stopToParentReceiveMs: activeStopReceivedAtMs - activeStopAtMs,
      mediaRecorderState: mediaRecorder ? mediaRecorder.state : "null",
    });
    if (mediaRecorder && mediaRecorder.state === "recording") {
      mediaRecorder.stop();
    } else {
      voiceLog("stopRecording-no-active-recorder", {
        requestId: currentRequestId(),
      });
      deliverTranscript("");
    }
  };

  toolbarBtn.addEventListener("click", () => {
    const state = toolbarBtn.dataset.state;
    if (state === "recording") {
      setToolbarState("processing");
      stopRecording(Date.now(), "toolbar-click");
    } else if (state === "idle") {
      startRecording("toolbar");
    }
    // ignore clicks while starting or processing
  });

  window.addEventListener("keydown", (ev) => {
    if (ev.key === "d" && ev.shiftKey && (ev.metaKey || ev.ctrlKey)) {
      ev.preventDefault();
      toolbarBtn.click();
    }
  });

  // iframe-driven flow: voice-start begins recording on the iframe's behalf;
  // voice-stop stops the (single in-flight) recording.
  window.addEventListener("message", (ev) => {
    const d = ev.data;
    if (!d || d.type !== "right-pane") return;
    if (d.kind === "voice-start") {
      voiceLog("iframe-voice-start", { requestId: d.requestId });
      startRecording({ source: ev.source, requestId: d.requestId });
    } else if (d.kind === "voice-stop") {
      const stopAtMs = typeof d.stopAtMs === "number" ? d.stopAtMs : Date.now();
      voiceLog("iframe-voice-stop", {
        requestId: d.requestId,
        stopAtMs: stopAtMs,
        stopToParentReceiveMs: Date.now() - stopAtMs,
      });
      if (
        !active ||
        active === "toolbar" ||
        !d.requestId ||
        !ev.source ||
        active.source !== ev.source ||
        active.requestId !== d.requestId
      ) {
        voiceLog("iframe-voice-stop-ignored", {
          requestId: d.requestId,
          stopAtMs: stopAtMs,
          activeWas:
            active === null ? null : active === "toolbar" ? "toolbar" : "iframe",
          activeRequestId: currentRequestId(),
          hasSource: !!ev.source,
          sourceMatches:
            !!(active && typeof active === "object" && ev.source && active.source === ev.source),
        });
        try {
          ev.source &&
            ev.source.postMessage(
              {
                type: "voice-into-result",
                requestId: d.requestId,
                transcript: "",
                stopAtMs: stopAtMs,
                stopToDeliverMs: Date.now() - stopAtMs,
              },
              "*",
            );
        } catch (_) {}
        return;
      }
      stopRecording(stopAtMs, "iframe-message");
    }
  });
})();
