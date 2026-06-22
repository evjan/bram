// Shell-side helpers exposed to any XMLUI app served by Bram
// binary. Include from your project's index.html with:
//
//   <script src="tauri://localhost/__shell/helpers.js"></script>
//
// Both iframes (right pane and agent-tools drawer) are same-origin with
// the parent shell at tauri://localhost, so these helpers call Tauri IPC
// directly via window.parent.__TAURI__.core.invoke. `getTauriInvoke()`
// formalizes the lookup with a window.__TAURI__ → window.parent → window.top
// fallback chain. The legacy postMessage bridge to app/main.js has been
// retired; voice recording (voiceStart / voiceStop) is the one remaining
// exception, because the parent shell owns the MediaRecorder pipeline.

window._xsLogs = window._xsLogs || [];

// Persist the tools-pane route across iframe reloads. main.js reassigns
// tools.src on every tools-pane-reload event (drawer code changed under
// app/tools/), which drops the hash and lands the user on the default
// route (Worklist). We solve this from inside the iframe: restore the
// saved hash on boot, save the current hash on change.
//
// Scoped to the tools iframe — user-project apps in the right pane have
// their own route conventions and should not be affected.
(function persistToolsRoute() {
  if (window.location.pathname.indexOf("/tools/") === -1) return;
  var key = "bram.tools.route";
  var legacyKey = "xmlui-desktop.tools.route";
  var bootedAt = Date.now();
  var STARTUP_SUPPRESS_MS = 1500;
  function trace(subkind, fields) {
    setTimeout(function () {
      try {
        if (typeof window.logToHost !== "function") return;
        var payload = {
          kind: "iframe-trace",
          subkind: subkind,
          at: new Date().toISOString(),
        };
        if (fields && typeof fields === "object") {
          Object.assign(payload, fields);
        }
        window.logToHost(payload);
      } catch (e) {}
    }, 0);
  }
  try {
    var current = window.location.hash;
    var saved = localStorage.getItem(key) || localStorage.getItem(legacyKey) || "";
    trace("tools-route-boot", {
      current: current || "",
      saved: saved,
      pathname: window.location.pathname || "",
    });
    if (!current || current === "#/") {
      if (saved && saved !== "#/") {
        window.location.hash = saved;
        trace("tools-route-restore", {
          from: current || "",
          route: saved,
        });
      }
    }
    // react-router-dom uses history.pushState which doesn't fire
    // hashchange, so poll instead of listening.
    setInterval(function () {
      var h = window.location.hash;
      var stored = localStorage.getItem(key) || "";
      if (
        h === "#/" &&
        stored &&
        stored !== "#/" &&
        Date.now() - bootedAt < STARTUP_SUPPRESS_MS
      ) {
        trace("tools-route-skip-root-save", {
          stored: stored,
          elapsedMs: Date.now() - bootedAt,
        });
        return;
      }
      if (h && h !== localStorage.getItem(key)) {
        localStorage.setItem(key, h);
        trace("tools-route-save", {
          route: h,
          previous: stored,
          elapsedMs: Date.now() - bootedAt,
        });
      }
    }, 500);
  } catch (e) {}
})();

// Main-thread heartbeat for the drawer iframe. setInterval scheduled
// every 200ms; if the actual gap exceeds the threshold the main thread
// was blocked (typically by heavy Markdown re-renders during JSONL
// cascade — the same condition that delays the inflightClaim
// DataSource's onLoaded handler). Emits one record per blockage with
// drift_ms, so a swallowed click can be correlated with main-thread
// busyness in bram-trace.log. Scoped to the drawer because that's
// where worklist clicks live; the right pane is a separate iframe with
// its own load profile.
(function heartbeat() {
  if (window.location.pathname.indexOf("/tools/") === -1) return;
  setTimeout(function () {
    try {
      window.logToHost && window.logToHost({
        kind: "iframe-trace",
        subkind: "helpers-js-loaded",
        build: "batch-v2",
        at: new Date().toISOString(),
      });
    } catch (e) {}
  }, 500);
  var TICK_MS = 200;
  // Threshold is configurable via appGlobals.heartbeatDriftThresholdMs
  // (see config.json). Defaults to 500ms when unset. Lower values
  // catch sub-second blockages at the cost of more records during
  // normal hot-render bursts.
  var DRIFT_THRESHOLD_MS =
    (window.appGlobals && Number(window.appGlobals.heartbeatDriftThresholdMs)) || 500;
  var last = performance.now();
  var batch = { fires: 0, sumDrift: 0, maxDrift: 0, spikes: 0, sinceMs: 0 };
  // Batch summary every 50 fires (~10s nominal). Emits aggregate
  // drift stats so we can see overall main-thread health independent
  // of individual spike records.
  function batchTick(drift) {
    if (batch.fires === 0) batch.sinceMs = Date.now();
    batch.fires += 1;
    batch.sumDrift += drift;
    if (drift > batch.maxDrift) batch.maxDrift = drift;
    if (drift >= DRIFT_THRESHOLD_MS) batch.spikes += 1;
    if (batch.fires >= 50) {
      // Gate: skip the emit while a PTY menu is pending.
      // window.__bramMenuPending mirrors bramAgentMenu (set by
      // Globals.xs applyAgentMenu). Reset still runs so a fresh
      // window starts post-dismiss.
      if (!window.__bramMenuPending) {
        try {
          window.logToHost({
            kind: "iframe-trace",
            subkind: "heartbeat-batch",
            fires: batch.fires,
            spanMs: Date.now() - batch.sinceMs,
            sumDriftMs: Math.round(batch.sumDrift),
            avgDriftMs: Math.round(batch.sumDrift / batch.fires),
            maxDriftMs: Math.round(batch.maxDrift),
            spikes: batch.spikes,
            at: new Date().toISOString(),
          });
        } catch (e) {}
      }
      batch = { fires: 0, sumDrift: 0, maxDrift: 0, spikes: 0, sinceMs: 0 };
    }
  }
  setInterval(function () {
    var now = performance.now();
    var drift = now - last - TICK_MS;
    last = now;
    batchTick(drift);
    if (drift >= DRIFT_THRESHOLD_MS && !window.__bramMenuPending) {
      try {
        window.logToHost({
          kind: "iframe-trace",
          subkind: "heartbeat-drift",
          drift_ms: Math.round(drift),
          at: new Date().toISOString(),
        });
      } catch (e) {}
    }
  }, TICK_MS);
})();

// Capture-phase click listener on `document` for the drawer iframe.
// Fires for EVERY click that reaches the DOM, BEFORE XMLUI's own
// onClick handlers. Distinguishes "click reached document but XMLUI's
// onClick didn't run" from "click never registered at all" — the
// former produces a `dom-click` record without a matching XMLUI
// `subkind=click`, pointing at button-disabled/re-rendered/dead-space
// failure modes that helpers.js can't otherwise detect. Capture phase
// (true) ensures this runs before bubbling-phase handlers.
(function captureClicks() {
  if (window.location.pathname.indexOf("/tools/") === -1) return;
  document.addEventListener("click", function (e) {
    try {
      var t = e.target;
      var tagName = t && t.tagName;
      var ariaLabel = (t && t.getAttribute && t.getAttribute("aria-label")) || "";
      var role = (t && t.getAttribute && t.getAttribute("role")) || "";
      var disabled = !!(t && t.disabled);
      window.logToHost({
        kind: "iframe-trace",
        subkind: "dom-click",
        tagName: String(tagName || ""),
        ariaLabel: String(ariaLabel),
        role: String(role),
        disabled: disabled,
        x: e.clientX,
        y: e.clientY,
        at: new Date().toISOString(),
      });
    } catch (le) {}
  }, true);
})();

// Outbound right-pane → PTY intents route through `queue_pty_intent`
// (#86), which appends to `resources/.pty-intent.jsonl` and drains
// under a process-wide mutex. The disk hop keeps each click durably
// recorded even if the iframe context is unsettled when the IPC fires
// — the host drains independently of the originating iframe state.
//
// `toShell` / `toTurn` / `sendKeys` keep their application-level
// responsibilities (whitespace normalization in `toTurn`, the
// implicit "\n" semantic in `toShell`, the "no framing" contract in
// `sendKeys`); PTY framing (bracketed-paste markers around toTurn
// data, trailing newline for toShell) is applied host-side in the
// drain so the right pane stays ignorant of terminal escape
// sequences.
// Write per-item feedback to resources/feedback-drafts/<feedbackId>.md
// without going through the PTY paste channel. toTurn collapses every
// whitespace run into a single space (line 227) and the receiving TUI's
// bracketed-paste buffer has its own content limits, so long Iterate
// feedback can lose structure or get truncated. Iterate now writes the
// feedback to disk via this helper and sends only a small feedbackRef
// in the toTurn payload; the agent reads the draft file directly. See
// #144.
window.queueFeedbackDraft = function (feedbackId, text) {
  var id = String(feedbackId || "");
  var s = String(text == null ? "" : text);
  // stage=source: what the iframe got from the textbox. stage=sink:
  // what was passed to the invoke. Identical lengths confirm no
  // client-side mangling; a delta points at iframe-side regression.
  try {
    window.logToHost({
      kind: "iframe-trace",
      subkind: "feedback-draft-write",
      stage: "source",
      feedback_id: id,
      source_bytes: s.length,
      at: new Date().toISOString(),
    });
  } catch (e) {}
  var invoke = getTauriInvoke();
  if (!invoke) return Promise.resolve(false);
  try {
    invoke("log_from_right_pane", {
      payload: {
        kind: "iframe-trace",
        subkind: "feedback-draft-write",
        stage: "sink",
        feedback_id: id,
        sink_bytes: s.length,
        at: new Date().toISOString(),
      },
    }).catch(function () {});
  } catch (e) {}
  return invoke("queue_feedback_draft", { payload: { feedback_id: id, text: s } })
    .then(function () {
      return true;
    })
    .catch(function (e) {
      console.error("queueFeedbackDraft invoke", e);
      try {
        window.logToHost({
          kind: "iframe-trace",
          subkind: "feedback-draft-write-failed",
          feedback_id: id,
          error: String((e && e.message) || e),
          at: new Date().toISOString(),
        });
      } catch (le) {}
      return false;
    });
};

window.sendIterateWithFeedbackDraft = function (items, selectedId, text) {
  var feedbackId = Date.now() + "-" + selectedId;
  window.queueFeedbackDraft(feedbackId, text).then(function (wroteDraft) {
    window.toTurn("iterate: " + JSON.stringify({
      items: (items || []).filter(function (i) { return i.id === selectedId; })
        .map(function (i) {
          return wroteDraft
            ? { id: i.id, hash: i.hash, feedbackRef: feedbackId }
            : { id: i.id, hash: i.hash, feedback: text };
        }),
    }));
  });
};

window.toShell = function (text) {
  var s = String(text);
  // Trace the entry so #86's "click swallowed" diagnostic flow can
  // distinguish between "helper never invoked" (no trace line) and
  // "helper invoked but queue / drain lost" (trace line present but
  // no [pty-intent] op=enqueue follows). kind: "iframe-trace" routes
  // through log_from_right_pane's iframe-trace branch into the
  // [iframe] category of resources/bram-traces/bram-trace.log.
  try {
    window.logToHost({
      kind: "iframe-trace",
      subkind: "to-shell",
      stage: "source",
      textLength: s.length,
      textPreview: s.slice(0, 80),
      at: new Date().toISOString(),
    });
  } catch (e) {}
  var invoke = getTauriInvoke();
  if (!invoke) return;
  invoke("queue_pty_intent", { payload: { kind: "toShell", data: s } }).catch(function (e) {
    console.error("toShell queue_pty_intent", e);
    try {
      window.logToHost({
        kind: "iframe-trace",
        subkind: "to-shell-invoke-failed",
        error: String((e && e.message) || e),
        at: new Date().toISOString(),
      });
    } catch (le) {}
  });
};
window.toTurn = function (text) {
  var s = String(text);
  try {
    window.logToHost({
      kind: "iframe-trace",
      subkind: "to-turn",
      stage: "source",
      textLength: s.length,
      textPreview: s.slice(0, 80),
      at: new Date().toISOString(),
    });
  } catch (e) {}
  var normalized = s.replace(/\s+/g, " ").trim();
  var invoke = getTauriInvoke();
  if (!invoke) return;
  invoke("log_from_right_pane", {
    payload: {
      kind: "iframe-trace",
      subkind: "to-turn",
      stage: "sink",
      textLength: normalized.length,
      textPreview: normalized.slice(0, 80),
      at: new Date().toISOString(),
    },
  }).catch(function () {});
  invoke("queue_pty_intent", { payload: { kind: "toTurn", data: normalized } }).catch(function (e) {
    console.error("toTurn queue_pty_intent", e);
    try {
      window.logToHost({
        kind: "iframe-trace",
        subkind: "to-turn-invoke-failed",
        error: String((e && e.message) || e),
        at: new Date().toISOString(),
      });
    } catch (le) {}
  });
};
// sendKeys writes raw bytes to the PTY with NO trailing newline (unlike
// toShell which always appends \n). Use it for control sequences like ESC,
// arrow keys, or single-keypress menu shortcuts.
window.sendKeys = function (text) {
  var invoke = getTauriInvoke();
  if (!invoke) return;
  invoke("queue_pty_intent", { payload: { kind: "sendKeys", data: String(text) } }).catch(function (e) {
    console.error("sendKeys queue_pty_intent", e);
    try {
      window.logToHost({
        kind: "iframe-trace",
        subkind: "send-keys-invoke-failed",
        error: String((e && e.message) || e),
        at: new Date().toISOString(),
      });
    } catch (le) {}
  });
};
window.recordToolbarPendingMenuFromEvent = function (event) {
  window.__bramToolbarMenuState = {
    present: !!(event && event.payload),
    atMs: Date.now(),
  };
};
window.getToolbarPendingMenuState = function () {
  return window.__bramToolbarMenuState || { present: false, atMs: 0 };
};
// Toolbar PTY subscribers. Invoked via xs delegators in Globals.xs.
//
// Originally migrated in commit d532432 step 5: the xs declarations
// were removed and Main.xmlui's bare-name calls were expected to
// resolve directly to `window.setToolbarPendingMenuFromEvent` etc.
// — that worked for the toolbar onClick handlers where the call is a
// top-level expression, but XMLUI's expression engine analyzes
// identifiers inside arrow-function bodies passed to
// subscribeTauriEvent and silently aborts the registration when a
// bare name has no xs declaration. Main.xmlui's onInit then stopped
// running its remaining statements partway through (statement 5
// onward), AgentMenu's mount cascade was disrupted, and menus
// stopped appearing. The fix: distinct __bram-prefixed window
// helpers paired with thin xs delegators below — the same pattern
// every other migrated function uses.
window.__bramSetToolbarPendingMenuFromEvent = function (e) {
  window.recordToolbarPendingMenuFromEvent(e);
};
window.__bramSetToolbarPendingMenuFromTurnState = function (turnState) {
  window.recordToolbarPendingMenuFromEvent({ payload: turnState && turnState.pendingMenu });
};
window.__bramTraceToolbarKey = function (key) {
  var state = window.getToolbarPendingMenuState();
  window.__bramIframeTrace("toolbar-key", {
    key: key,
    menuPresent: state.present ? 1 : 0,
    menuAgeMs: state.atMs ? (Date.now() - state.atMs) : -1,
  });
};
window.logToHost = function (payload) {
  // Master-flag short-circuit. Paired with `window.iframeTrace`
  // below. When traces are off, skip the Tauri IPC invoke (the
  // dominant per-event cost). Default-ON so behavior is preserved
  // during the brief startup window before the self-init fetch
  // below resolves the actual setting.
  if (window.__bramTracesEnabled === false) return;
  var invoke = getTauriInvoke();
  if (!invoke) return;
  invoke("log_from_right_pane", { payload: payload }).catch(function () {});
};

// iframeTrace: the [iframe] category of the comms-path trace log
// (issue #49). Forwards a structured record to the host's
// `log_from_right_pane` Tauri command, which routes records whose
// `kind` is `"iframe-trace"` into resources/bram-traces/bram-trace.log
// when BRAM_TRACE=1 is set on the host. No-op when logToHost isn't
// wired up. subkind is a token from the spec's maintained vocabulary
// (click, inflight-set, inflight-clear, listener-fired, ...); fields
// are arbitrary per-event metadata.
//
// Lives in plain JS so callers from XMLUI-evaluated arrow function
// bodies and xs functions don't pay the per-statement-await cost of
// processStatementQueueAsync
// (xmlui/src/components-core/script-runner/process-statement-async.ts:115-166).
// The xs declaration in Globals.xs is a thin delegator that calls
// this; the window helper uses the `__bram` prefix to avoid the
// trap where xs's `function iframeTrace` declaration overwrites
// `window.iframeTrace` (browser scripts hoist top-level function
// declarations onto window), which would turn the delegator's
// `window.iframeTrace(...)` call into recursion-to-itself. Same
// pattern as `window.__bramApplyAgentMenu` paired with the xs
// `applyAgentMenu` delegator (commit ea9480e).
window.__bramIframeTrace = function (subkind, fields) {
  try {
    if (window.__bramTracesEnabled === false) return;
    if (typeof window.logToHost !== "function") return;
    var payload = { kind: "iframe-trace", subkind: subkind, at: new Date().toISOString() };
    if (fields && typeof fields === "object") {
      Object.assign(payload, fields);
    }
    window.logToHost(payload);
  } catch (e) {}
};

// Cascade-diagnosis instrumentation (refs #93). Emits a helper-call
// record when a hot JSONL-walking helper exceeds the threshold. Cheap
// paths (no-op early returns, cache hits) don't log because their _t0
// measurement is sub-ms. Threshold deliberately low to catch
// sub-frame stalls that compound across the cascade.
window.__bramTraceHelperTiming = function (name, t0, extra) {
  try {
    var elapsed = (typeof performance !== "undefined" && performance.now)
      ? performance.now() - t0
      : Date.now() - t0;
    if (elapsed < 2) return;
    if (typeof window.logToHost !== "function") return;
    var payload = {
      kind: "iframe-trace",
      subkind: "helper-call",
      name: name,
      ms: Math.round(elapsed),
      at: new Date().toISOString(),
    };
    if (extra && typeof extra === "object") Object.assign(payload, extra);
    window.logToHost(payload);
  } catch (e) {}
};

// Plain-JS equivalents of XMLUI's xs-only readLocalStorage /
// writeLocalStorage built-ins
// (xmlui/src/components-core/appContext/local-storage-functions.ts).
// Same dot-path semantics: the first segment is the localStorage entry
// name, remaining segments are a property path inside the parsed JSON
// object. Used by the __bram-prefixed localStorage shim helpers below
// so they can run in plain JS without re-entering XMLUI's statement
// queue. `bram.worklistMessageDraft` reads
// `JSON.parse(localStorage.bram).worklistMessageDraft`. Splitter keys
// like `bram.splitter.worklist` are two-level.
function __bramSplitKey(key) {
  var s = String(key);
  var dot = s.indexOf(".");
  return dot === -1 ? [s, undefined] : [s.substring(0, dot), s.substring(dot + 1)];
}

function __bramReadLS(key, fallback) {
  try {
    var parts = __bramSplitKey(key);
    var raw = localStorage.getItem(parts[0]);
    if (raw === null) return fallback;
    var root;
    try { root = JSON.parse(raw); } catch (e) { return fallback; }
    if (parts[1] === undefined) return root;
    var sub = parts[1].split(".");
    var cur = root;
    for (var i = 0; i < sub.length; i++) {
      if (cur == null || typeof cur !== "object") return fallback;
      cur = cur[sub[i]];
    }
    return cur === undefined ? fallback : cur;
  } catch (e) { return fallback; }
}

function __bramWriteLS(key, value) {
  try {
    var parts = __bramSplitKey(key);
    if (parts[1] === undefined) {
      if (value === undefined) localStorage.removeItem(parts[0]);
      else localStorage.setItem(parts[0], JSON.stringify(value));
      return;
    }
    var raw = localStorage.getItem(parts[0]);
    var root;
    if (raw === null) {
      root = {};
    } else {
      try { root = JSON.parse(raw); } catch (e) { root = {}; }
      if (!root || typeof root !== "object") root = {};
    }
    var sub = parts[1].split(".");
    var cur = root;
    for (var i = 0; i < sub.length - 1; i++) {
      var k = sub[i];
      if (!cur[k] || typeof cur[k] !== "object") cur[k] = {};
      cur = cur[k];
    }
    var last = sub[sub.length - 1];
    if (value === undefined) delete cur[last];
    else cur[last] = value;
    localStorage.setItem(parts[0], JSON.stringify(root));
  } catch (e) {}
}

// Worklist "message agent" persistence + lifecycle shims. Counterparts
// for the xs delegators in Globals.xs (audit step 3, 2026-06-14).
// Each is invoked through bare-name `restoreWorklistDraft(...)` from
// xmlui markup or other xs code, which resolves to the xs delegator,
// which routes here. The cost saving is per-call body collapse: each
// of these used to run through processStatementQueueAsync's 3-await
// loop for every statement in the body; now the entire body runs as
// one plain-JS function call (one xs statement total).

var __bramWorklistDraftPersistTimer = null;
var __bramWorklistDraftPending = null;

function __bramFlushWorklistDraft() {
  if (__bramWorklistDraftPersistTimer) {
    clearTimeout(__bramWorklistDraftPersistTimer);
    __bramWorklistDraftPersistTimer = null;
  }
  if (__bramWorklistDraftPending !== null) {
    __bramWriteLS("bram.worklistMessageDraft", __bramWorklistDraftPending);
    __bramWorklistDraftPending = null;
  }
}

window.__bramRestoreWorklistDraft = function () {
  return __bramReadLS("bram.worklistMessageDraft", "");
};

window.__bramPersistWorklistDraft = function (text) {
  __bramWorklistDraftPending = String(text || "");
  if (__bramWorklistDraftPersistTimer) clearTimeout(__bramWorklistDraftPersistTimer);
  __bramWorklistDraftPersistTimer = setTimeout(__bramFlushWorklistDraft, 400);
};

window.__bramClearWorklistDraft = function () {
  if (__bramWorklistDraftPersistTimer) {
    clearTimeout(__bramWorklistDraftPersistTimer);
    __bramWorklistDraftPersistTimer = null;
  }
  __bramWorklistDraftPending = null;
  __bramWriteLS("bram.worklistMessageDraft", "");
};

window.__bramFlushWorklistDraft = __bramFlushWorklistDraft;

window.addEventListener("beforeunload", __bramFlushWorklistDraft);

window.__bramRestoreConversationOpen = function () {
  var raw = __bramReadLS("bram.conversationOpen", "1");
  var result = raw !== "0";
  window.__bramIframeTrace("conversation-open-restore", { raw: raw, open: result });
  return result;
};

window.__bramToggleConversationOpen = function (current) {
  var next = !current;
  __bramWriteLS("bram.conversationOpen", next ? "1" : "0");
  window.__bramIframeTrace("conversation-open-save", { open: next });
  return next;
};

window.__bramRestoreWorklistConversationLayout = function () {
  var raw = __bramReadLS("bram.worklistConversationLayout", "");
  if (raw === "split" || raw === "worklist" || raw === "conversation") {
    window.__bramIframeTrace("worklist-conversation-layout-restore", { raw: raw, layout: raw });
    return raw;
  }
  var openRaw = __bramReadLS("bram.conversationOpen", "1");
  var migrated = "split";
  window.__bramIframeTrace("worklist-conversation-layout-restore", { raw: raw, conversationOpen: openRaw, layout: migrated });
  return migrated;
};

window.__bramSetWorklistConversationLayout = function (layout) {
  var next = (layout === "worklist" || layout === "conversation") ? layout : "split";
  __bramWriteLS("bram.worklistConversationLayout", next);
  // Keep the older boolean key coherent for existing traces/helpers.
  __bramWriteLS("bram.conversationOpen", next === "worklist" ? "0" : "1");
  window.__bramIframeTrace("worklist-conversation-layout-save", { layout: next });
  return next;
};

// Worklist UI state model is now multi-expand: any number of items can be
// "open" simultaneously, each with its own feedback-draft text. State shape:
//   { expandedItemIds: string[], feedbackDraftsById: Record<string, string> }
// Legacy fields (selected, expandedItemId, feedbackExpanded, selectedFeedback)
// are honored on read for migration from pre-sticky-expansion sessions; they
// are never written back. After the first save in the new shape, the legacy
// keys disappear.
window.__bramReadWorklistUiStateObject = function () {
  var raw = __bramReadLS("bram.worklistUiState", "");
  if (!raw) return {};
  var saved;
  if (typeof raw === "object") {
    saved = raw;
  } else {
    try { saved = JSON.parse(raw); } catch (e) { saved = null; }
  }
  return (saved && typeof saved === "object") ? saved : {};
};

window.__bramRestoreWorklistUiState = function (field) {
  var saved = window.__bramReadWorklistUiStateObject();
  if (field === "expandedItemIds") {
    // New canonical field. Fall back to legacy single-id on first migration.
    var arr = Array.isArray(saved.expandedItemIds) ? saved.expandedItemIds.slice() : null;
    if (!arr) {
      var legacy = saved.expandedItemId || saved.selected || null;
      arr = legacy ? [legacy] : [];
    }
    window.__bramIframeTrace("worklist-ui-state-restore", { field: field, count: arr.length });
    return arr;
  }
  if (field === "feedbackDraftsById") {
    // New canonical field. Migrate legacy { selected, selectedFeedback }.
    var map = (saved.feedbackDraftsById && typeof saved.feedbackDraftsById === "object")
      ? Object.assign({}, saved.feedbackDraftsById)
      : null;
    if (!map) {
      map = {};
      if (saved.selected && saved.selectedFeedback) {
        map[saved.selected] = String(saved.selectedFeedback);
      }
    }
    window.__bramIframeTrace("worklist-ui-state-restore", { field: field, count: Object.keys(map).length });
    return map;
  }
  // Legacy single-value fields retained for any stragglers; new code shouldn't read these.
  if (field === "feedbackExpanded") return !!saved.feedbackExpanded;
  if (field === "selectedFeedback") return String(saved.selectedFeedback || "");
  if (field === "selected") return saved.selected || null;
  if (field === "expandedItemId") return saved.expandedItemId || null;
  return null;
};

window.__bramPersistWorklistUiState = function (state) {
  // state: { expandedItemIds: string[], feedbackDraftsById: Record<string, string> }
  var ids = (state && Array.isArray(state.expandedItemIds)) ? state.expandedItemIds.slice() : [];
  var drafts = (state && state.feedbackDraftsById && typeof state.feedbackDraftsById === "object") ? state.feedbackDraftsById : {};
  // Garbage-collect drafts whose item is no longer expanded — keeps storage bounded.
  var prunedDrafts = {};
  for (var i = 0; i < ids.length; i++) {
    var id = ids[i];
    if (drafts[id]) prunedDrafts[id] = String(drafts[id]);
  }
  window.__bramIframeTrace("worklist-ui-state-save", {
    expandedCount: ids.length,
    draftCount: Object.keys(prunedDrafts).length,
  });
  __bramWriteLS("bram.worklistUiState", JSON.stringify({
    expandedItemIds: ids,
    feedbackDraftsById: prunedDrafts,
  }));
};

window.__bramClearWorklistUiState = function () {
  window.__bramIframeTrace("worklist-ui-state-clear", {});
  __bramWriteLS("bram.worklistUiState", "");
};

window.__bramRestoreWorklistAwaiting = function () {
  var flag = __bramReadLS("bram.awaitingResponse", "");
  var setAtRaw = __bramReadLS("bram.awaitingResponseSetAt", "");
  var setAt = parseInt(setAtRaw, 10);
  if (flag === "1" && !isNaN(setAt) && (Date.now() - setAt) < 300000) {
    return true;
  }
  __bramWriteLS("bram.awaitingResponse", "");
  __bramWriteLS("bram.awaitingResponseSetAt", "");
  return false;
};

window.__bramRestoreWorklistAwaitingSetAt = function () {
  var setAtRaw = __bramReadLS("bram.awaitingResponseSetAt", "");
  var setAt = parseInt(setAtRaw, 10);
  return isNaN(setAt) ? 0 : setAt;
};

window.__bramMarkAwaitingStarted = function () {
  var now = Date.now();
  __bramWriteLS("bram.awaitingResponse", "1");
  __bramWriteLS("bram.awaitingResponseSetAt", String(now));
  return now;
};

window.__bramRestoreWorklistSubmittedMessage = function () {
  return __bramReadLS("bram.worklistSubmittedMessage", "");
};

window.__bramRestoreWorklistSubmittedKind = function () {
  var kind = __bramReadLS("bram.worklistSubmittedKind", "");
  return kind === "message" || kind === "action" ? kind : null;
};

window.__bramSetWorklistSubmittedKind = function (kind) {
  if (kind === "message" || kind === "action") {
    __bramWriteLS("bram.worklistSubmittedKind", kind);
  } else {
    __bramWriteLS("bram.worklistSubmittedKind", "");
  }
  return kind || null;
};

window.__bramRestoreWorklistSubmittedBaseline = function () {
  var raw = __bramReadLS("bram.worklistSubmittedBaseline", "");
  var n = parseInt(raw, 10);
  return isNaN(n) ? 0 : n;
};

window.__bramClearWorklistAwaiting = function (clearDraft) {
  __bramWriteLS("bram.awaitingResponse", "");
  __bramWriteLS("bram.awaitingResponseSetAt", "");
  window.__bramSetWorklistSubmittedKind(null);
  if (clearDraft) {
    __bramWriteLS("bram.worklistMessageDraft", "");
  }
};

window.__bramRestoreSplitterSize = function (key, fallback) {
  var raw = __bramReadLS("bram.splitter." + key, "");
  var s = String(raw || "").trim();
  var n = parseFloat(s);
  var hasUnit = /(?:px|%)$/i.test(s);
  var result = (!isNaN(n) && n > 0)
    ? (hasUnit ? s : (n < 100 ? (n + "%") : (n + "px")))
    : fallback;
  window.__bramIframeTrace("splitter-restore", { key: key, raw: raw, result: result });
  return result;
};

window.__bramSaveSplitterSize = function (key, sizes) {
  if (Array.isArray(sizes)) {
    var a = Number(sizes[0]);
    var b = Number(sizes[1]);
    var total = a + b;
    var pct = total > 0 ? (a / total) * 100 : 0;
    window.__bramIframeTrace("splitter-save", { key: key, sizes: sizes, pct: pct, unit: "%" });
    if (pct > 0 && pct < 100) {
      __bramWriteLS("bram.splitter." + key, String(Math.round(pct * 10) / 10) + "%");
    }
    return;
  }
  var px = Number(sizes);
  window.__bramIframeTrace("splitter-save", { key: key, sizes: sizes, px: px, unit: "px" });
  if (px > 0) {
    __bramWriteLS("bram.splitter." + key, String(Math.round(px)) + "px");
  }
};

// Body strings for the Settings tab info dialogs. Lifted out of
// Settings.xmlui to keep the markup readable; the dialog itself
// stays inline in Settings since it's a single consumer.
window.settingsInfoBodies = {
  agentCommand:
    "## Agent command\n\n" +
    "Typed into the PTY shell at spawn — bash parses it, so flags work " +
    "(claude --continue, codex resume, etc.). Restart Bram for changes " +
    "to take effect.",
  batchCommitActions:
    "## Batch commit actions\n\n" +
    "Adds Approve all / Drop all controls to the Worklist tab, shown only " +
    "when 2 or more TO COMMIT (applied) items are present. Approve all " +
    "authorizes the agent to commit every TO COMMIT item in one turn; the " +
    "agent picks commit granularity (typically one bundled commit for a " +
    "coordinated change). Drop all removes every TO COMMIT item from the " +
    "worklist, but the on-disk file edits stay (same as a single Drop) — " +
    "ask the agent to discard them if you want them gone. TO APPLY items " +
    "are unaffected. Issues flagged via closesIssues are not auto-closed " +
    "in a batch; close them via a single-item Approve or ask the agent.",
  ui:
    "## Show or Hide Target App\n\n" +
    "Usually off. Most people run their app in their own browser, so the " +
    "target-app pane stays hidden and the agent pane fills the space. Turn " +
    "it on to preview a simple app inside Bram; turn it off to reclaim the " +
    "room." +
    "\n\n## Agent-pane hot-reload\n\n" +
    "Only matters when developing Bram itself: when on, the agent pane " +
    "reloads automatically as you edit Bram’s own source. Leave it off " +
    "otherwise.",
  traces:
    "## Tracing enabled\n\n" +
    "Master switch for writes to " +
    "resources/bram-traces/bram-trace.log. When off, every [emit] / " +
    "[iframe] / [route] line is a no-op regardless of the Inspector " +
    "trace tap below. If BRAM_TRACE is set in the environment at " +
    "launch (e.g. BRAM_TRACE=1 cargo run), it wins and this switch is " +
    "ignored — so CI / shell wrappers keep behaving the same." +
    "\n\n## Inspector trace tap\n\n" +
    "Forwards XMLUI Inspector events " +
    "(window._xsLogs) from the agent pane into bram-trace.log as " +
    "[iframe] subkind=inspector-event, so they interleave with host " +
    "traces live (no Inspector export needed). Capped at 50 entries " +
    "per 200ms tick; overflow emits subkind=inspector-overflow. " +
    "Inspector traces are intentionally complete and noisy (one per " +
    "keystroke, etc.); future work will add per-category filters. " +
    "Requires Tracing enabled above." +
    "\n\n" +
    "Both persist in .bram.json under traces.*.",
};

// "claude code" for the claude provider, raw provider name otherwise.
// Falls back through mainAgentStatus.provider →
// enhanceStatus.activeProvider → '' so the idle state still gets a
// label. Guards mainAgentStatus against null (idle case).
window.providerDisplayName = function (mainAgentStatus, enhanceStatusValue) {
  var p =
    (mainAgentStatus && mainAgentStatus.provider) ||
    (enhanceStatusValue && enhanceStatusValue.activeProvider) ||
    "";
  return p === "claude" ? "claude code" : p;
};

// Should the idle-state provider label be visible? True when we have
// some agent state, we're NOT currently working or finished, and
// there's a provider name available to display.
window.shouldShowIdleProvider = function (mainAgentStatus, enhanceStatusValue) {
  if (!mainAgentStatus && !enhanceStatusValue) return false;
  if (mainAgentStatus &&
      (mainAgentStatus.state === "working" || mainAgentStatus.state === "finished")) {
    return false;
  }
  return Boolean(
    (mainAgentStatus && mainAgentStatus.provider) ||
    (enhanceStatusValue && enhanceStatusValue.activeProvider)
  );
};

// "<provider> <verb>…" for the working state.
window.headerWorkingLabel = function (mainAgentStatus, enhanceStatusValue) {
  var s = mainAgentStatus || {};
  var verb = s.verb || "working";
  return window.providerDisplayName(mainAgentStatus, enhanceStatusValue) + " " + verb + "…";
};

// "<provider> <verb> · <elapsed>" for the finished state. Verb
// fall-through: status.verb (when finished) → status.verb (when
// non-working) → lastSeenAgentVerb (when non-working) → "Finished".
window.headerFinishedLabel = function (mainAgentStatus, enhanceStatusValue, lastSeenAgentVerb) {
  var s = mainAgentStatus || {};
  var verb;
  if (s.state === "finished") {
    verb = s.verb || "Finished";
  } else if (s.verb && s.verb !== "working") {
    verb = s.verb;
  } else if (lastSeenAgentVerb && lastSeenAgentVerb !== "working") {
    verb = lastSeenAgentVerb;
  } else {
    verb = "Finished";
  }
  var base = window.providerDisplayName(mainAgentStatus, enhanceStatusValue) + " " + verb;
  return base + (s.elapsedText ? " · " + s.elapsedText : "");
};

// Compute the next sort state for a clickable table-header. If the
// column is already active, flip the direction; otherwise switch to
// the new column with its default direction. Returns {field, dir}.
window.toggleSort = function (currentField, currentDir, newField, defaultDir) {
  if (currentField === newField) {
    return { field: newField, dir: currentDir === "asc" ? "desc" : "asc" };
  }
  return { field: newField, dir: defaultDir };
};

// Render a table-header label with an active-column arrow.
// "STATE ↑" / "STATE ↓" if currentField matches; "STATE" otherwise.
window.sortLabel = function (label, currentField, currentDir, fieldName) {
  if (currentField !== fieldName) return label;
  return label + (currentDir === "asc" ? " ↑" : " ↓");
};

// Select the list to display in a searchable tab. If query is 2+
// chars, return the search results (accepting either the raw-array
// shape Sessions uses or the {results} wrapper used elsewhere).
// Otherwise return the full list. Used by Feedback, History, Issues,
// Sessions.
window.selectDisplayed = function (query, searchValue, fullList) {
  if (query && query.trim().length >= 2) {
    if (Array.isArray(searchValue)) return searchValue;
    return (searchValue && searchValue.results) || [];
  }
  return fullList || [];
};

// Normalize a path/URL for an XMLUI Image's src binding. Pass through
// data: and http(s) URLs verbatim; otherwise route through the
// /__file?path= shim with optional file://(localhost)? prefix stripped.
// Used by every Image preview in the agent pane.
window.imageSrcForPath = function (path) {
  var p = path || "";
  if (p.startsWith("data:") || p.startsWith("http")) return p;
  var cleaned = p.startsWith("file://")
    ? p.replace(/^file:\/\/(localhost)?/, "")
    : p;
  return "/__file?path=" + encodeURIComponent(cleaned);
};

// extractImagePaths — promoted from local to window in step 9 so other
// window.__bram* helpers (sessionTurns / _parseLinesToTurns chain) can
// share the same regex compile.
window.__bramExtractImagePaths = function (text) {
  if (!text) return [];
  var paths = [];
  var imagePath = "(?:/[^\\]]+|[A-Za-z]:\\\\[^\\]]+)\\.(?:png|jpg|jpeg|gif|webp)";
  var re = new RegExp("\\[Image: source: (" + imagePath + ")\\]", "gi");
  var m;
  while ((m = re.exec(text)) !== null) paths.push(m[1]);
  return paths;
};
function __bramExtractImagePaths(text) {
  // Kept as a local alias so the step-3 submission trio above (defined
  // before the window helper) still resolves.
  return window.__bramExtractImagePaths(text);
}

// Submission trio. submitWorklistMessageFast needs the xs-side
// voiceTarget (still an xs var; step 4 will mirror it onto window).
// For now the xs delegator passes it as the third argument.
window.__bramSubmitWorklistMessageFast = function (text, voiceTarget) {
  if (!text || !text.trim()) return false;
  var userTyped = text.trim();
  var toSend = window.__bramWithStagedImageMarkers(userTyped, "message-agent", voiceTarget);
  var sentAt = Date.now();
  window.__bramIframeTrace("message-agent-submit", { stage: "before-toTurn", chars: toSend.length, sentAt: sentAt });
  if (typeof window.toTurn === "function") window.toTurn(toSend);
  window.__bramIframeTrace("message-agent-submit", { stage: "after-toTurn", chars: toSend.length, sentAt: sentAt });
  var baseline = 0;
  __bramWriteLS("bram.worklistMessageDraft", "");
  __bramWriteLS("bram.worklistSubmittedMessage", userTyped);
  __bramWriteLS("bram.worklistSubmittedBaseline", String(baseline || 0));
  window.__bramSetWorklistSubmittedKind("message");
  return { message: userTyped, images: __bramExtractImagePaths(toSend), baseline: baseline, sentAtText: new Date().toLocaleTimeString() };
};

window.__bramWithStagedImageMarkers = function (text, target, voiceTarget) {
  var requestedTarget = target || voiceTarget || "";
  var consumeTarget = requestedTarget;
  if (requestedTarget === "feedback") {
    var focusedFeedback = window.bramActiveFocusedFeedbackItemIdMirror || "";
    if (focusedFeedback) {
      consumeTarget = "feedback:" + focusedFeedback;
    } else if (window.bramCurrentPasteTarget) {
      consumeTarget = window.bramCurrentPasteTarget() || requestedTarget;
    }
  }
  bramTracePasteImage("with-markers", {
    requestedTarget: requestedTarget,
    voiceTarget: voiceTarget || "",
    consumeTarget: consumeTarget,
    pendingBefore: bramPendingPastedImageSummary()
  });
  var paths = window.bramConsumePastedImagePaths
    ? window.bramConsumePastedImagePaths(consumeTarget)
    : [];
  if (!paths || paths.length === 0) return text;
  var lines = paths.map(function (p) { return "Read this screenshot: @" + p + "\n[Image: source: " + p + "]"; });
  var markers = lines.join("\n");
  var skipPrefix = "skip-worklist:";
  var trimmedStart = (text || "").trimStart();
  if (trimmedStart.indexOf(skipPrefix) === 0) {
    var leading = text.slice(0, text.length - trimmedStart.length);
    var rest = trimmedStart.slice(skipPrefix.length).trimStart();
    return leading + skipPrefix + " " + markers + (rest ? "\n\n" + rest : "");
  }
  return text ? markers + "\n\n" + text : markers;
};

// Pure predicate — voice-target whitelist for text-input destinations.
// xs delegator in Globals.xs preserves the bare-name callability.
window.__bramIsWorklistTextVoiceTarget = function (target) {
  return ["message-agent", "feedback", "new-item", "new-issue"].indexOf(target || "") !== -1;
};

// Inflight + submitted-message helpers (audit step 6). All pure data
// transforms; xs delegators in Globals.xs preserve bare-name calls.
window.__bramInflightActionLabel = function (kind) {
  if (kind === "approved") return "Approving";
  if (kind === "iterate") return "Iterating";
  if (kind === "drop") return "Dropping";
  return "";
};

window.__bramStripImageMarkerPrefix = function (text) {
  return (text || "").replace(/^(\s*Read this screenshot: @\S+\s*)+/, "").trim();
};

window.__bramWorklistSubmittedMatches = function (exchangeUserText, submitted) {
  if (!submitted) return false;
  var a = window.__bramStripImageMarkerPrefix(exchangeUserText || "").replace(/\s+/g, " ").trim();
  var b = window.__bramStripImageMarkerPrefix(submitted || "").replace(/\s+/g, " ").trim();
  return a === b;
};

// Canonical turn-end observer. Returns true iff the caller should run
// the five-line clear sequence (awaitingResponse + submittedKind +
// liveSubmittedAssistantText + liveSubmittedAssistantKey +
// clearWorklistAwaiting). Emits the trace line itself so callers stay
// uniform — `clear-awaiting` on a true result, `mark-turn-ended-skipped`
// with an explicit reason on a false result. Replaces the four
// duplicated guards previously embedded in Workspace.xmlui's
// agent-turn-end, agent-turn-killed, inflight-clear, and
// agent-status state=finished subscribers.
window.__bramMarkTurnEnded = function (via, state) {
  state = state || {};
  var awaitingResponse = !!state.awaitingResponse;
  if (!awaitingResponse) return false;
  var submitting = !!state.submitting;
  var setAt = state.awaitingResponseSetAt || 0;
  var submittedKind = state.submittedKind || "";
  var sinceSet = setAt ? (Date.now() - setAt) : -1;
  if (submitting) {
    window.__bramIframeTrace("mark-turn-ended-skipped", { via: via, reason: "action-in-flight", sinceSetMs: sinceSet, submittedKind: submittedKind });
    return false;
  }
  if (sinceSet >= 0 && sinceSet < 750) {
    window.__bramIframeTrace("mark-turn-ended-skipped", { via: via, reason: "within-window", sinceSetMs: sinceSet, submittedKind: submittedKind });
    return false;
  }
  window.__bramIframeTrace("clear-awaiting", { via: via, sinceSetMs: sinceSet, submittedKind: submittedKind });
  return true;
};

// Conversation-sync computation extracted from Workspace.xmlui's mega
// ChangeListener. Takes a snapshot of the relevant state and returns a
// decision object describing what the xs caller should mutate. Emits the
// conversation-sync and (exchange-match) clear-awaiting trace lines
// internally so the xs caller stays bookkeeping-only.
//
// Returns:
//   {
//     sigChanged: bool,
//     signature: string | undefined,    // only when sigChanged
//     liveAssistantCapture: {text, key} | null,
//     shouldClearAwaiting: bool
//   }
//
// Note: the original markup at Workspace.xmlui:294 had an inner
// `submittedKind === 'message' && submitting` branch INSIDE the
// clear-awaiting block, but the preceding line set submittedKind to null
// so the inner branch could never fire. That dead code is not carried
// forward here; behavior is identical (the branch never fired before,
// it doesn't fire now).
window.__bramComputeConversationSync = function (state) {
  state = state || {};
  var lastExchange = state.lastExchange || {};
  var submitted = (state.submittedWorklistMessage || "").trim();
  var exchangeUser = (lastExchange.userText || "").trim();
  var exchangeAssistant = (lastExchange.assistantText || "").trim();
  var exchangeUserImages = lastExchange.userImages || [];
  var exchangeAssistantImages = lastExchange.assistantImages || [];
  var exchangeTools = (lastExchange.tools || []).slice(-3);
  var exchangeMatchesSubmitted = window.__bramWorklistSubmittedMatches(exchangeUser, submitted);
  var awaitingResponse = !!state.awaitingResponse;
  var submittedKind = state.submittedKind || "";
  var liveSubmittedAssistantText = state.liveSubmittedAssistantText || "";
  var liveSubmittedAssistantKey = state.liveSubmittedAssistantKey || "";
  var stickyConversationTools = state.stickyConversationTools || [];
  var displayTools = exchangeTools.length > 0 ? exchangeTools : stickyConversationTools;
  var user = (awaitingResponse && submitted) ? submitted : (exchangeUser || submitted);

  var liveAssistantCapture = null;
  if (awaitingResponse && submittedKind === "message" && submitted && exchangeMatchesSubmitted && exchangeAssistant) {
    liveAssistantCapture = { text: exchangeAssistant, key: submitted };
    liveSubmittedAssistantText = exchangeAssistant;
    liveSubmittedAssistantKey = submitted;
  }

  var liveSubmittedAssistant = liveSubmittedAssistantKey === submitted ? liveSubmittedAssistantText : "";
  var assistant = awaitingResponse
    ? (exchangeMatchesSubmitted ? (exchangeAssistant || liveSubmittedAssistant) : "")
    : (exchangeAssistant || state.lastAssistantStableText || "").trim();
  var source = awaitingResponse
    ? (exchangeMatchesSubmitted ? (exchangeAssistant ? "last-exchange" : "awaiting") : "awaiting")
    : (exchangeAssistant ? "last-exchange" : (assistant ? "last-assistant-text" : "none"));
  var toolsSig = (awaitingResponse && !exchangeMatchesSubmitted)
    ? ""
    : displayTools.map(function (t) { return (t.id || "") + ":" + (t.name || "") + ":" + (t.summary || "") + ":" + (t.errored ? "1" : "0"); }).join("|");
  var imageSig = exchangeUserImages.join("|") + "\n---\n" + exchangeAssistantImages.join("|");
  var sig = user + "\n---\n" + toolsSig + "\n---\n" + assistant + "\n---\n" + imageSig;

  var sigChanged = sig !== (state.lastConversationSig || "");
  if (sigChanged) {
    window.__bramIframeTrace("conversation-sync", {
      source: source,
      entries: (assistant ? 1 : 0) + (toolsSig ? displayTools.length : 0),
      userImages: exchangeUserImages.length,
      assistantImages: exchangeAssistantImages.length,
      matchesSubmitted: !!exchangeMatchesSubmitted,
      submittedKind: submittedKind,
      awaiting: awaitingResponse,
      rawToolsLen: exchangeTools.length,
      displayToolsLen: displayTools.length,
      gateFires: !!(awaitingResponse && submittedKind !== "action" && !exchangeMatchesSubmitted)
    });
  }

  var shouldClearAwaiting = !!(awaitingResponse && exchangeMatchesSubmitted && assistant && state.agentTurnEndedSinceSubmit);
  if (shouldClearAwaiting) {
    window.__bramIframeTrace("clear-awaiting", {
      via: "exchange-match",
      sinceSetMs: state.awaitingResponseSetAt ? (Date.now() - state.awaitingResponseSetAt) : -1,
      stableLen: (state.lastAssistantStableText || "").length,
      exchangeUserLen: exchangeUser.length,
      exchangeAssistantLen: exchangeAssistant.length,
      submittedLen: submitted.length
    });
  }

  return {
    sigChanged: sigChanged,
    signature: sigChanged ? sig : undefined,
    liveAssistantCapture: liveAssistantCapture,
    shouldClearAwaiting: shouldClearAwaiting
  };
};

// Plain-JS equivalent of xs `App.mark(label)`. App.mark pushes a
// `kind: "app:mark"` record to the Inspector buffer at window._xsLogs
// (xmlui/src/components-core/appContext/app-utils.ts:49-53). The
// pure-JS helpers below preserve the marks so Inspector exports stay
// comparable across the migration.
function __bramAppMark(label) {
  try {
    if (!window._xsLogs) return;
    var perfTs = (typeof performance !== "undefined" && performance.now) ? performance.now() : 0;
    window._xsLogs.push({ kind: "app:mark", ts: Date.now(), label: label, perfTs: perfTs });
  } catch (e) {}
}

window.__bramFormatUserTurnForTranscript = function (text) {
  if (!text) return "";
  var stripped = text.replace(/^(voice|talk):\s*/, "");
  if (stripped !== text) return stripped;
  var m = text.match(/^(approved|drop|iterate):\s*([\s\S]*)$/);
  if (m) {
    try {
      var data = JSON.parse(m[2]);
      return window.__bramWorklistActionDisplay(m[1], data.items || data.ids || []);
    } catch (e) {
      return text;
    }
  }
  return text;
};

window.__bramWorklistActionStatusLabel = function (item) {
  var status = (item && item.status) || "proposed";
  if (status === "applied") return "To Commit";
  if (status === "proposed") return "To Apply";
  return status ? status.charAt(0).toUpperCase() + status.slice(1) : "Worklist";
};

window.__bramConversationPaneUserText = function (text) {
  if (!text) return "";
  var stripped = text.replace(/^(voice|talk):\s*/, "");
  if (stripped !== text) return stripped;
  var clean = window.__bramStripImageMarkerPrefix(stripped);
  var m = clean.match(/^(approved|drop|iterate):\s*([\s\S]*)$/);
  if (!m) return clean;
  var kind = m[1];
  try {
    var data = JSON.parse(m[2]);
    var items = data.items || data.ids || [];
    var action = window.__bramWorklistActionDisplay(kind, items);
    var feedbacks = items
      .map(function (it) { return (it && typeof it === "object" && it.feedback) ? String(it.feedback).trim() : ""; })
      .filter(function (s) { return s.length > 0; });
    if (feedbacks.length === 0) return action;
    return action + "\n\n" + feedbacks.join("\n\n");
  } catch (e) {
    return clean;
  }
};

window.__bramWorklistActionDisplay = function (kind, items) {
  var action =
    kind === "approved" ? "Approved" :
    kind === "iterate" ? "Iterated" :
    kind === "drop" ? "Dropped" :
    "Submitted";
  var ids = (items || []).map(function (i) {
    if (typeof i === "string") return i;
    return (i && i.id) || "";
  }).filter(Boolean);
  if (ids.length === 0) return action;
  if (ids.length === 1) return action + " " + ids[0];
  return action + " " + ids.length + " items: " + ids.join(", ");
};

window.__bramWorklistActionStatusSuffix = function (item) {
  var status = (item && item.status) || "proposed";
  if (status === "applied") return " to commit";
  if (status === "proposed") return " to apply";
  return "";
};

window.__bramWorklistActionConversationDisplay = function (kind, items, selectedId, feedback) {
  var selected = (items || []).filter(function (i) { return i.id === selectedId; });
  var suffix = selected.length === 1 ? window.__bramWorklistActionStatusSuffix(selected[0]) : "";
  return window.__bramWorklistActionDisplay(kind, selected) + suffix;
};

window.__bramTraceIterateEnabled = function (submitting, selected, selectedFeedback) {
  __bramAppMark("iterate-enabled");
  return !submitting && !!selected && (selectedFeedback || "").trim().length > 0;
};

window.__bramTraceApproveDropEnabled = function (submitting, selected) {
  __bramAppMark("approve-drop-enabled");
  return !submitting && !!selected;
};

window.__bramBuildApprovePayload = function (items, selectedId, feedback) {
  __bramAppMark("build-approve-payload");
  return JSON.stringify({
    items: (items || []).filter(function (i) { return i.id === selectedId; })
      .map(function (i) { return { id: i.id, hash: i.hash, feedback: feedback }; }),
  });
};

window.__bramBuildIteratePayload = function (items, selectedId, feedback) {
  __bramAppMark("build-iterate-payload");
  // feedback may be either an inline string (backward-compat) or a
  // `{ feedbackRef: "<id>" }` object (new, from queueFeedbackDraft).
  return JSON.stringify({
    items: (items || []).filter(function (i) { return i.id === selectedId; })
      .map(function (i) {
        return feedback && typeof feedback === "object" && feedback.feedbackRef
          ? { id: i.id, hash: i.hash, feedbackRef: feedback.feedbackRef }
          : { id: i.id, hash: i.hash, feedback: feedback };
      }),
  });
};

window.__bramBuildDropPayload = function (items, selectedId, feedback) {
  __bramAppMark("build-drop-payload");
  return JSON.stringify({
    items: (items || []).filter(function (i) { return i.id === selectedId; })
      .map(function (i) { return { id: i.id, hash: i.hash, feedback: feedback }; }),
  });
};

window.__bramBuildSingleItemApprovePayload = function (itemRef, feedback) {
  __bramAppMark("build-single-item-approve-payload");
  return JSON.stringify({
    items: [{ id: itemRef.id, hash: itemRef.hash, feedback: feedback }],
  });
};

window.__bramCountByStatus = function (items, status) {
  return (items || []).filter(function (i) { return (i.status || "proposed") === status; }).length;
};

window.__bramBuildBatchApprovePayload = function (items, feedback) {
  __bramAppMark("build-batch-approve-payload");
  return JSON.stringify({
    items: (items || []).filter(function (i) { return (i.status || "proposed") === "applied"; })
      .map(function (i) { return { id: i.id, hash: i.hash, feedback: feedback || "" }; }),
  });
};

window.__bramBuildBatchDropPayload = function (items, feedback) {
  __bramAppMark("build-batch-drop-payload");
  return JSON.stringify({
    items: (items || []).filter(function (i) { return (i.status || "proposed") === "applied"; })
      .map(function (i) { return { id: i.id, hash: i.hash, feedback: feedback || "" }; }),
  });
};

window.__bramPrepareBatchWorklistActionSubmission = function (opts) {
  opts = opts || {};
  var items = opts.items || [];
  var kind = opts.kind === "drop" ? "drop" : "approved";
  var target = kind === "drop" ? "drop-all" : "approve-all";
  var ids = items.filter(function (i) { return (i.status || "proposed") === "applied"; });
  window.__bramIframeTrace("click", { target: target, count: ids.length });
  window.__bramClearWorklistUiState();
  var submittedItemId = ids.length > 0 ? ids[0].id : null;
  var submittedKind = window.__bramSetWorklistSubmittedKind("action");
  window.__bramIframeTrace("inflight-set", { item: submittedItemId, via: "click", target: target });
  return {
    turnText: (kind === "drop" ? "drop: " : "approved: ") + (
      kind === "drop"
        ? window.__bramBuildBatchDropPayload(items, "")
        : window.__bramBuildBatchApprovePayload(items, "")
    ),
    submitting: true,
    submittedItemId: submittedItemId,
    submittedKind: submittedKind,
    actionProgressScope: "batch",
    actionProgressKind: kind,
    actionProgressTick: 0,
    expandedItemIds: [],
    feedbackDraftsById: {},
  };
};

// Step 9 — sessionTurns bundle. Image / markdown / tool parsers +
// JSONL → turns parser + sessionTurns with its function-property memo.
// All pure data transforms; internal calls dispatch to window.__bram*
// directly so the whole chain stays in plain JS.

window.__bramRewriteXmluiDocUrls = function (text) {
  if (!text) return text;
  return text
    .replace(/https:\/\/docs\.xmlui\.org\/components\//g, "https://www.xmlui.org/docs/reference/components/")
    .replace(/https:\/\/docs\.xmlui\.org\//g, "https://www.xmlui.org/docs/");
};

window.__bramStripImagePaths = function (text) {
  if (!text) return text;
  var imagePath = "(?:/[^\\]]+|[A-Za-z]:\\\\[^\\]]+)\\.(?:png|jpg|jpeg|gif|webp)";
  return text
    .replace(new RegExp("\\n*\\[Image: source: " + imagePath + "\\]", "gi"), "")
    .replace(/^(\s*Read this screenshot: @\S+\s*)+/, "")
    .trim();
};

window.__bramExtractMarkdownImages = function (text) {
  if (!text) return [];
  var urls = [];
  var md = /!\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g;
  var m;
  while ((m = md.exec(text)) !== null) urls.push(m[1]);
  var html = /<img\b[^>]*\bsrc=["']([^"']+)["'][^>]*>/gi;
  while ((m = html.exec(text)) !== null) urls.push(m[1]);
  return urls;
};

window.__bramStripMarkdownImages = function (text) {
  if (!text) return text;
  return text
    .replace(/\n*!\[[^\]]*\]\([^)\s]+(?:\s+"[^"]*")?\)/g, "")
    .replace(/\n*<img\b[^>]*\bsrc=["'][^"']+["'][^>]*>/gi, "");
};

window.__bramToolSummary = function (name, input) {
  if (!input || typeof input !== "object") return name || "";
  if (name === "Edit" || name === "MultiEdit") {
    return (input.file_path || "") + " edited";
  }
  if (name === "Write") {
    var lines = (input.content || "").split("\n").length;
    return (input.file_path || "") + " — wrote " + lines + " line" + (lines === 1 ? "" : "s");
  }
  if (name === "Bash") {
    var cmd = input.command || "";
    return cmd.length > 80 ? cmd.slice(0, 80) + "…" : cmd;
  }
  if (name === "Read") {
    var s = input.file_path || "";
    if (input.offset || input.limit) {
      var start = input.offset || 1;
      s += ":" + start;
      if (input.limit) s += "-" + (start + input.limit - 1);
    }
    return s;
  }
  if (name === "Grep" || name === "Glob") {
    return (input.pattern || "") + (input.path ? " in " + input.path : "");
  }
  if (name === "Task" || name === "Agent") {
    return (input.subagent_type || "") + (input.description ? " — " + input.description : "");
  }
  return name || "";
};

window.__bramParseJsonString = function (value) {
  if (typeof value !== "string") return null;
  try {
    return JSON.parse(value);
  } catch (e) {
    return null;
  }
};

window.__bramCodexToolName = function (payload) {
  if (!payload) return "";
  if (payload.namespace) return payload.namespace.replace(/^mcp__/, "") + "." + (payload.name || "");
  return payload.name || "";
};

window.__bramCodexToolInput = function (payload) {
  if (!payload) return {};
  if (payload.type === "function_call") {
    var parsed = window.__bramParseJsonString(payload.arguments);
    return parsed !== null ? parsed : (payload.arguments || {});
  }
  if (payload.type === "custom_tool_call") {
    var parsed2 = window.__bramParseJsonString(payload.input);
    return parsed2 !== null ? parsed2 : (payload.input || "");
  }
  return {};
};

window.__bramCodexToolSummary = function (payload) {
  if (!payload) return "";
  var name = window.__bramCodexToolName(payload);
  var input = window.__bramCodexToolInput(payload);
  if (payload.name === "exec_command" && input && typeof input === "object" && input.cmd) {
    return input.cmd.length > 80 ? input.cmd.slice(0, 80) + "…" : input.cmd;
  }
  if (payload.name === "write_stdin" && input && typeof input === "object") {
    var chars = input.chars || "";
    var session = input.session_id ? ("session " + input.session_id) : "stdin";
    if (!chars) return session;
    var label = chars === "" ? "Esc" : chars.replace(/\r/g, "\\r").replace(/\n/g, "\\n");
    return session + " ← " + (label.length > 40 ? label.slice(0, 40) + "…" : label);
  }
  if (payload.name === "apply_patch" && typeof input === "string") {
    var m = input.match(/\*\*\* (?:Add|Update|Delete) File: ([^\n]+)/);
    return m ? (m[1] + " patch") : "patch";
  }
  if (name.indexOf("filesystem.") === 0 && input && typeof input === "object" && input.path) {
    return input.path;
  }
  if (name.indexOf("xmlui.") === 0 && input && typeof input === "object") {
    return input.path || input.component || input.query || name;
  }
  if (input && typeof input === "object") return window.__bramToolSummary(payload.name || name, input);
  return name;
};

window.__bramToolInputJsonLines = function (input, maxLines) {
  var cap = maxLines || 20;
  if (input === null || input === undefined) return { lines: [], remaining: 0 };
  if (typeof input === "string") {
    var allStr = input.split("\n");
    return { lines: allStr.slice(0, cap), remaining: Math.max(0, allStr.length - cap) };
  }
  var json;
  try {
    json = JSON.stringify(input, null, 2);
  } catch (e) {
    return { lines: ["(unserializable input)"], remaining: 0 };
  }
  var all = json.split("\n");
  return { lines: all.slice(0, cap), remaining: Math.max(0, all.length - cap) };
};

window.__bramToolResultText = function (content) {
  if (!content) return "";
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .filter(function (c) { return c && c.type === "text"; })
      .map(function (c) { return c.text || ""; })
      .join("\n");
  }
  return "";
};

window.__bramIsErrorResult = function (block) {
  if (!block) return false;
  if (block.is_error) return true;
  var text = window.__bramToolResultText(block.content);
  return text.indexOf("Error:") === 0 || text.indexOf("<tool_use_error>") === 0;
};

window.__bramCodexToolOutput = function (payload) {
  if (!payload || (payload.type !== "function_call_output" && payload.type !== "custom_tool_call_output")) {
    return null;
  }
  var raw = payload.output;
  if (typeof raw !== "string") return { text: "", errored: false };
  var parsed = window.__bramParseJsonString(raw);
  if (parsed && typeof parsed === "object") {
    var text = typeof parsed.output === "string"
      ? parsed.output
      : typeof parsed.stderr === "string"
        ? parsed.stderr
        : raw;
    var exitCode = parsed.metadata && typeof parsed.metadata.exit_code === "number"
      ? parsed.metadata.exit_code
      : null;
    return { text: text, errored: exitCode !== null && exitCode !== 0 };
  }
  var exitMatch = raw.match(/Process exited with code (\d+)/);
  var ec = exitMatch ? parseInt(exitMatch[1], 10) : 0;
  return { text: raw, errored: !!exitMatch && ec !== 0 };
};

window.__bramTurnsLooselyEqual = function (a, b) {
  if (!a || !b) return false;
  if (a.role !== b.role) return false;
  if (a.text !== b.text) return false;
  var ae = a.entries || [], be = b.entries || [];
  if (ae.length !== be.length) return false;
  for (var i = 0; i < ae.length; i++) {
    var x = ae[i], y = be[i];
    if (!x || !y) return false;
    if (x.kind !== y.kind) return false;
    if (x.kind === "text") {
      if (x.text !== y.text) return false;
    } else {
      if (x.id !== y.id) return false;
      if (!!x.errored !== !!y.errored) return false;
    }
  }
  var ai = a.images || [], bi = b.images || [];
  if (ai.length !== bi.length) return false;
  return true;
};

window.__bramParseLinesToTurns = function (lines, toolIndex) {
  toolIndex = toolIndex || {};
  var turns = [];
  for (var li = 0; li < lines.length; li++) {
    var line = lines[li];
    if (!line) continue;
    var r;
    try { r = JSON.parse(line); } catch (e) { continue; }
    var role = null;
    var entries = [];
    var inlineImages = [];
    if (r.type === "user" || r.type === "assistant") {
      if (!r.message || !r.message.content) continue;
      role = r.type;
      var content = r.message.content;
      if (typeof content === "string") {
        if (content) entries.push({ kind: "text", text: content });
      } else if (Array.isArray(content)) {
        for (var ci = 0; ci < content.length; ci++) {
          var c = content[ci];
          if (!c) continue;
          if (c.type === "text" && c.text) {
            entries.push({ kind: "text", text: c.text });
          } else if (c.type === "tool_use") {
            var entry = {
              kind: "tool",
              id: c.id,
              name: c.name,
              summary: window.__bramToolSummary(c.name, c.input || {}),
            };
            entries.push(entry);
            if (c.id) toolIndex[c.id] = entry;
          } else if (c.type === "tool_result") {
            var matching = c.tool_use_id && toolIndex[c.tool_use_id];
            if (matching) {
              matching.errored = window.__bramIsErrorResult(c);
              if (matching.errored) {
                var txt = window.__bramToolResultText(c.content);
                matching.errorText = txt.split("\n")[0].slice(0, 200);
              }
            }
          } else if (c.type === "image" && c.source && c.source.type === "base64" && c.source.data) {
            var mt = c.source.media_type || "image/png";
            inlineImages.push("data:" + mt + ";base64," + c.source.data);
          }
        }
      }
    } else if (r.type === "event_msg" && r.payload) {
      if (r.payload.type === "user_message") role = "user";
      if (r.payload.type === "agent_message") role = "assistant";
      var t = r.payload.message || "";
      if (t) entries.push({ kind: "text", text: t });
    } else if (r.type === "response_item" && r.payload) {
      var p = r.payload;
      if (p.type === "function_call" || p.type === "custom_tool_call") {
        role = "assistant";
        var entry2 = {
          kind: "tool",
          id: p.call_id,
          name: window.__bramCodexToolName(p),
          summary: window.__bramCodexToolSummary(p),
        };
        entries.push(entry2);
        if (p.call_id) toolIndex[p.call_id] = entry2;
      } else if (p.type === "function_call_output" || p.type === "custom_tool_call_output") {
        var matching2 = p.call_id && toolIndex[p.call_id];
        if (matching2) {
          var output = window.__bramCodexToolOutput(p);
          matching2.errored = !!(output && output.errored);
          if (output && output.text) {
            var firstLine = output.text.split("\n")[0].slice(0, 200);
            if (matching2.errored) matching2.errorText = firstLine;
          }
        }
      }
    }
    if (!role) continue;
    if (entries.length === 0 && inlineImages.length === 0) continue;
    var originalJoined = entries.filter(function (e) { return e.kind === "text"; })
      .map(function (e) { return e.text; }).join("\n\n");
    var pathsFromText = window.__bramExtractImagePaths(originalJoined);
    for (var ei = 0; ei < entries.length; ei++) {
      var e2 = entries[ei];
      if (e2.kind === "text") {
        e2.text = window.__bramStripImagePaths(window.__bramRewriteXmluiDocUrls(e2.text));
      }
    }
    var textJoined = entries.filter(function (e) { return e.kind === "text"; })
      .map(function (e) { return e.text; }).join("\n\n");
    if (role === "user" && inlineImages.length === 0 && entries.every(function (e) { return e.kind === "text"; })
        && /^(\[Image: source: [^\]]+\]\s*)+$/.test(originalJoined.trim())) continue;
    if (entries.length === 0 && inlineImages.length === 0) continue;
    turns.push({
      role: role,
      text: textJoined,
      entries: entries,
      images: inlineImages.length > 0 ? inlineImages : pathsFromText,
    });
  }
  return turns;
};

// sessionTurns with function-property memoization. Cache lives on the
// window.__bramSessionTurns function object itself (._cacheKey /
// ._cacheValue / ._parseCount), same shape as the xs original. Polled
// JSONL identity check + incremental parse via prefix-extension are
// preserved exactly.
window.__bramSessionTurns = function (jsonlText) {
  var fn = window.__bramSessionTurns;
  if (!jsonlText) return fn._cacheValue || [];
  if (fn._cacheKey === jsonlText && fn._cacheValue) {
    return fn._cacheValue;
  }
  var prevKey = fn._cacheKey;
  var prevValue = fn._cacheValue;
  var now0 = (typeof performance !== "undefined" && performance.now) ? performance.now() : Date.now();
  if (prevKey && prevValue && jsonlText.length > prevKey.length &&
      jsonlText.substring(0, prevKey.length) === prevKey) {
    var suffix = jsonlText.substring(prevKey.length);
    var toolIndex = {};
    for (var ti = 0; ti < prevValue.length; ti++) {
      var t = prevValue[ti];
      var es = t.entries || [];
      for (var ei = 0; ei < es.length; ei++) {
        var e = es[ei];
        if (e && e.kind === "tool" && e.id) toolIndex[e.id] = e;
      }
    }
    var newTurns = window.__bramParseLinesToTurns(suffix.split("\n"), toolIndex);
    fn._cacheKey = jsonlText;
    fn._cacheValue = newTurns.length > 0 ? prevValue.concat(newTurns) : prevValue;
    fn._parseCount = (fn._parseCount || 0) + 1;
    var now1 = (typeof performance !== "undefined" && performance.now) ? performance.now() : Date.now();
    var elapsed = now1 - now0;
    if (elapsed > 2 || newTurns.length > 0) {
      window.__bramIframeTrace("sessionTurns-parse", {
        ms: Math.round(elapsed),
        len: jsonlText.length,
        suffixLen: suffix.length,
        turns: fn._cacheValue.length,
        newTurns: newTurns.length,
        n: fn._parseCount,
        path: "incremental",
      });
    }
    return fn._cacheValue;
  }
  fn._parseCount = (fn._parseCount || 0) + 1;
  var turns = window.__bramParseLinesToTurns(jsonlText.split("\n"));
  var prev = fn._cacheValue || [];
  for (var i = 0; i < turns.length && i < prev.length; i++) {
    if (window.__bramTurnsLooselyEqual(turns[i], prev[i])) {
      turns[i] = prev[i];
    } else {
      break;
    }
  }
  fn._cacheKey = jsonlText;
  fn._cacheValue = turns;
  var now2 = (typeof performance !== "undefined" && performance.now) ? performance.now() : Date.now();
  var elapsed2 = now2 - now0;
  if (elapsed2 > 2) {
    window.__bramIframeTrace("sessionTurns-parse", {
      ms: Math.round(elapsed2),
      len: jsonlText.length,
      turns: turns.length,
      n: fn._parseCount,
      path: "full",
    });
  }
  return turns;
};

// History helpers (audit step 8). All pure. Internal calls go through
// the window.__bram* versions directly so the whole chain stays in
// plain JS (xs delegators below are entry points only).

window.__bramHistoryPhaseKind = function (phase) {
  var summary = ((phase && phase.summary) || "").toLowerCase();
  if (summary.indexOf("applied") >= 0) return "applied";
  if (summary.indexOf("proposed") >= 0) return "proposed";
  return "";
};

window.__bramHistoryDecodeJsonStringValue = function (raw) {
  if (!raw) return "";
  try {
    return JSON.parse('"' + raw + '"');
  } catch (err) {
    return raw.replace(/\\n/g, "\n").replace(/\\"/g, '"');
  }
};

window.__bramHistoryExtractProseFromDiff = function (diff) {
  var lines = (diff || "").split("\n");
  var before = "";
  var after = "";
  for (var i = 0; i < lines.length; i++) {
    var line = lines[i];
    var afterMatch = line.match(/^\+\s+"after":\s+"(.*)"[,]?$/);
    if (afterMatch) {
      after = window.__bramHistoryDecodeJsonStringValue(afterMatch[1].replace(/",?$/, ""));
      continue;
    }
    var beforeMatch = line.match(/^\+\s+"before":\s+"(.*)"[,]?$/);
    if (beforeMatch) {
      before = window.__bramHistoryDecodeJsonStringValue(beforeMatch[1].replace(/",?$/, ""));
    }
  }
  return after || before;
};

window.__bramHistoryLatestPhase = function (group) {
  var phases = (group && group.phases) || [];
  return phases.length > 0 ? phases[phases.length - 1] : null;
};

window.__bramHistoryCurrentItem = function (group) {
  return (group && group.currentItem) || null;
};

window.__bramHistoryItemProse = function (item) {
  if (!item) return "";
  var after = typeof item.after === "string" ? item.after.trim() : "";
  if (after) return after;
  var before = typeof item.before === "string" ? item.before.trim() : "";
  return before;
};

window.__bramHistoryCurrentProsePhase = function (group) {
  var item = window.__bramHistoryCurrentItem(group);
  var itemProse = window.__bramHistoryItemProse(item);
  if (itemProse) {
    return {
      phase: window.__bramHistoryLatestPhase(group),
      prose: itemProse,
      source: "snapshot",
    };
  }
  var phases = (group && group.phases) || [];
  for (var i = phases.length - 1; i >= 0; i--) {
    var prose = window.__bramHistoryExtractProseFromDiff(phases[i].diff || "");
    if (prose) {
      return { phase: phases[i], prose: prose, source: "diff" };
    }
  }
  return { phase: null, prose: "", source: "" };
};

window.__bramHistoryCardProsePreview = function (group) {
  var current = window.__bramHistoryCurrentProsePhase(group).prose || "";
  var normalized = current.replace(/\s+/g, " ").trim();
  if (!normalized) return "";
  if (normalized.length <= 240) return normalized;
  return normalized.slice(0, 237).trimEnd() + "...";
};

window.__bramHistoryDateParts = function (iso) {
  if (!iso) return { date: "", time: "" };
  var d = new Date(iso);
  if (isNaN(d.getTime())) {
    return { date: iso.slice(0, 10), time: iso.slice(11, 16) };
  }
  var pad = function (n) { return String(n).padStart(2, "0"); };
  return {
    date: d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate()),
    time: pad(d.getHours()) + ":" + pad(d.getMinutes()),
  };
};

window.__bramHistoryDateRangeLine = function (group) {
  var phases = (group && group.phases) || [];
  if (!phases.length) return "";
  var first = window.__bramHistoryDateParts((phases[0] || {}).iso || "");
  var last = window.__bramHistoryDateParts((phases[phases.length - 1] || {}).iso || "");
  if (first.date && first.date === last.date) {
    return "On " + first.date + " from " + first.time + " to " + last.time;
  }
  return "From " + first.date + " " + first.time + " to " + last.date + " " + last.time;
};

window.__bramHistoryPhaseLabel = function (phase) {
  var summary = ((phase && phase.summary) || "").toLowerCase();
  if (summary.indexOf("committed") >= 0) return "Committed";
  if (summary.indexOf("applied") >= 0) return "Applied";
  if (summary.indexOf("proposed") >= 0) return "Proposed";
  if (summary.indexOf("dropped") >= 0 || summary.indexOf("pruned") >= 0) return "Dropped";
  return (phase && phase.summary) || "Changed";
};

window.__bramHistoryPhasePath = function (group) {
  var phases = (group && group.phases) || [];
  var labels = [];
  for (var i = 0; i < phases.length; i++) {
    var label = window.__bramHistoryPhaseLabel(phases[i]);
    if (labels[labels.length - 1] !== label) labels.push(label);
  }
  return labels.join(" -> ");
};

window.__bramHistoryCommitUrl = function (group) {
  var phases = (group && group.phases) || [];
  for (var i = phases.length - 1; i >= 0; i--) {
    var phase = phases[i] || {};
    var summary = (phase.summary || "").toLowerCase();
    var url = typeof phase.commitUrl === "string" ? phase.commitUrl.trim() : "";
    if (url && summary.indexOf("committed") >= 0) return url;
  }
  return "";
};

window.__bramHistoryItemFieldMarkdown = function (group, field) {
  var item = window.__bramHistoryCurrentItem(group);
  var value = item && typeof item[field] === "string" ? item[field].trim() : "";
  return value || "";
};

window.__bramHistoryItemFilesLine = function (group) {
  var item = window.__bramHistoryCurrentItem(group);
  if (!item) return "";
  if (Array.isArray(item.files)) return item.files.join(", ");
  if (typeof item.file === "string") return item.file;
  return "";
};

window.__bramWorklistItemFiles = function (itemOrGroup) {
  var item = itemOrGroup;
  if (itemOrGroup && itemOrGroup.currentItem) {
    item = itemOrGroup.currentItem;
  }
  if (!item) return [];
  if (Array.isArray(item.files)) {
    return item.files
      .filter(function (file) {
        return typeof file === "string" && file.trim();
      })
      .map(function (file) { return file.trim(); });
  }
  if (typeof item.file === "string" && item.file.trim()) {
    return [item.file.trim()];
  }
  return [];
};

window.__bramHistoryLatestProseChanged = function (group) {
  var phase = window.__bramHistoryLatestPhase(group);
  var diff = (phase && phase.diff) || "";
  return diff.indexOf('"before"') >= 0 || diff.indexOf('"after"') >= 0;
};

window.__bramHistoryDraftWasMissing = function (group) {
  var item = window.__bramHistoryCurrentItem(group);
  return !!(item && item._draftMissing);
};

window.__bramHistoryItemFate = function (group) {
  var phases = (group && group.phases) || [];
  for (var i = phases.length - 1; i >= 0; i--) {
    var summary = ((phases[i] && phases[i].summary) || "").toLowerCase();
    if (summary.indexOf("committed") >= 0) return "Fate: committed.";
    if (summary.indexOf("dropped") >= 0 || summary.indexOf("pruned") >= 0) return "Fate: dropped.";
  }
  return "Fate: still active.";
};

window.__bramInflightSentinelDecide = function (data, prevSubmitting, prevSubmittedItemId) {
  var claimIds = (data && data.ids) || [];
  if (claimIds.length > 0) {
    var targeted = claimIds[0];
    var transitioning = !prevSubmitting || prevSubmittedItemId !== targeted;
    return {
      kind: "submit",
      submitting: transitioning ? true : prevSubmitting,
      submittedItemId: transitioning ? targeted : prevSubmittedItemId,
      actionProgressKind: (data && data.kind) || "",
    };
  } else if (prevSubmitting) {
    return {
      kind: "clear",
      trace: { reason: "sentinel-cleared", item: prevSubmittedItemId || "" },
    };
  }
  return { kind: "none" };
};

window.__bramRecordWorklistFeedbackConversation = function (text) {
  if (!text || !text.trim()) return false;
  var message = text.trim();
  var baseline = 0;
  __bramWriteLS("bram.worklistSubmittedMessage", message);
  __bramWriteLS("bram.worklistSubmittedBaseline", String(baseline));
  window.__bramSetWorklistSubmittedKind("action");
  return { message: message, images: __bramExtractImagePaths(message), baseline: baseline, sentAtText: new Date().toLocaleTimeString() };
};

window.__bramPrepareWorklistMessageSubmission = function (opts) {
  opts = opts || {};
  var rawText = opts.text || "";
  var skipWorklist = opts.mode === "skip-worklist";
  window.__bramWorklistMessageSubmissionSeq = (window.__bramWorklistMessageSubmissionSeq || 0) + 1;
  var seq = window.__bramWorklistMessageSubmissionSeq;
  if (skipWorklist && !rawText.trim()) return { submitted: false, seq: seq };
  var text = skipWorklist ? ("skip-worklist: " + rawText.trim()) : rawText;
  if (!text.trim()) return { submitted: false, seq: seq };

  if (window.__bramFlushWorklistDraft) window.__bramFlushWorklistDraft();
  var sent = window.__bramSubmitWorklistMessageFast(text);
  if (!sent) return { submitted: false, seq: seq };

  var pasteState = window.__bramPasteStateSnapshot(opts.voiceTarget || "message-agent");
  var submittedImages = sent.images || [];
  window.__bramIframeTrace("submitted-images", {
    kind: skipWorklist ? "message-skip-worklist" : "message",
    count: submittedImages.length,
    first: submittedImages[0] || "",
  });

  return {
    submitted: true,
    seq: seq,
    pendingPastedImageCount: pasteState.count,
    pendingPastedImagePaths: pasteState.paths,
    stagingPastedImageCount: pasteState.staging,
    stickyConversationTools: [],
    stickyConversationToolsKey: "",
    stickyConversationUserImages: [],
    stickyConversationUserImagesKey: "",
    liveSubmittedAssistantText: "",
    liveSubmittedAssistantKey: "",
    submittedWorklistImages: submittedImages,
    submittedWorklistMessage: sent.message,
    submittedTurnsBaseline: sent.baseline,
    messageSentAtText: sent.sentAtText,
    submittedKind: window.__bramSetWorklistSubmittedKind("message"),
    awaitingResponse: true,
    awaitingResponseSetAt: window.__bramMarkAwaitingStarted(),
  };
};

window.__bramPrepareWorklistActionSubmission = function (opts) {
  opts = opts || {};
  window.__bramWorklistActionSubmissionSeq = (window.__bramWorklistActionSubmissionSeq || 0) + 1;
  var seq = window.__bramWorklistActionSubmissionSeq;
  var kind = opts.kind || "";
  var items = opts.items || [];
  var selectedId = opts.selectedId || "";
  var pasteTarget = opts.pasteTarget || ("feedback:" + selectedId);
  var rawFeedback = opts.rawFeedback || "";
  var feedback = window.__bramWithStagedImageMarkers(rawFeedback, pasteTarget);
  var displayItems = opts.displayItems || items;
  var displayText = window.__bramWorklistActionConversationDisplay(kind, displayItems, selectedId, feedback);
  var sent = window.__bramRecordWorklistFeedbackConversation(feedback ? (displayText + "\n\n" + feedback) : displayText);
  var submittedImages = [];
  var awaitingResponse = false;
  var awaitingResponseSetAt = 0;

  if (sent) {
    submittedImages = ((sent.images && sent.images.length > 0) ? sent.images : window.__bramExtractImagePaths(feedback));
    window.__bramIframeTrace("submitted-images", {
      kind: "action",
      action: opts.imageAction || kind,
      count: submittedImages.length,
      first: submittedImages[0] || "",
    });
    awaitingResponse = true;
    awaitingResponseSetAt = window.__bramMarkAwaitingStarted();
  }

  if (opts.inflightTarget) {
    window.__bramIframeTrace("inflight-set", {
      item: selectedId,
      via: "click",
      target: opts.inflightTarget,
    });
  }

  var feedbackDraftsById = opts.feedbackDraftsById || {};
  var nextFeedbackDrafts = Object.assign({}, feedbackDraftsById);
  delete nextFeedbackDrafts[selectedId];
  window.__bramPersistWorklistUiState({
    expandedItemIds: opts.expandedItemIds || [],
    feedbackDraftsById: nextFeedbackDrafts,
  });

  var payloadFeedback = Object.prototype.hasOwnProperty.call(opts, "payloadFeedback")
    ? opts.payloadFeedback
    : feedback;
  var turnText = "";
  if (opts.payloadKind === "single-approve") {
    turnText = "approved: " + window.__bramBuildSingleItemApprovePayload(opts.itemRef, payloadFeedback);
  } else if (kind === "approved") {
    turnText = "approved: " + window.__bramBuildApprovePayload(items, selectedId, payloadFeedback);
  } else if (kind === "drop") {
    turnText = "drop: " + window.__bramBuildDropPayload(items, selectedId, payloadFeedback);
  }

  var pasteState = window.__bramPasteStateSnapshot(opts.voiceTarget || "message-agent");
  return {
    seq: seq,
    feedback: feedback,
    turnText: turnText,
    pendingPastedImageCount: pasteState.count,
    pendingPastedImagePaths: pasteState.paths,
    stagingPastedImageCount: pasteState.staging,
    submittedWorklistImages: submittedImages,
    submittedWorklistMessage: sent ? sent.message : "",
    submittedTurnsBaseline: sent ? sent.baseline : 0,
    messageSentAtText: sent ? sent.sentAtText : "",
    awaitingResponse: awaitingResponse,
    awaitingResponseSetAt: awaitingResponseSetAt,
    submittedItemId: selectedId,
    submittedKind: window.__bramSetWorklistSubmittedKind("action"),
    submitting: true,
    actionProgressKind: kind,
    actionProgressTick: 0,
    feedbackDraftsById: nextFeedbackDrafts,
  };
};

function __bramBuildCloseIssueLines(state) {
  var lines = [];
  Object.keys(state || {}).forEach(function (key) {
    var v = state[key];
    if (!v || !v.close) return;
    var comment = (v.comment || "").trim();
    if (comment) lines.push("close-issue: " + key + " comment: " + JSON.stringify(comment));
    else lines.push("close-issue: " + key);
  });
  return lines;
}

function __bramCombineFeedbackWithCloseLines(base, lines, pushBeforeClose) {
  var baseTrim = (base || "").trim();
  var generated = [];
  if (pushBeforeClose) generated.push("push-before-close: true");
  if (lines && lines.length > 0) generated.push.apply(generated, lines);
  if (generated.length === 0) return baseTrim;
  if (!baseTrim) return generated.join("\n");
  return baseTrim + "\n\n" + generated.join("\n");
}

window.__bramPrepareCloseIssueWorklistActionSubmission = function (opts) {
  opts = opts || {};
  var item = opts.item || {};
  var feedbackDraftsById = opts.feedbackDraftsById || {};
  var rawFeedback = feedbackDraftsById[item.id] || "";
  var pasteTarget = "feedback:" + item.id;
  var payloadFeedback = rawFeedback;
  var imageAction = "approved-no-close";

  if (opts.closeIssues) {
    payloadFeedback = __bramCombineFeedbackWithCloseLines(
      window.__bramWithStagedImageMarkers(rawFeedback, pasteTarget),
      __bramBuildCloseIssueLines(opts.closeIssuesState),
      true,
    );
    imageAction = "approved-close";
  }

  return window.__bramPrepareWorklistActionSubmission({
    kind: "approved",
    items: [item],
    displayItems: [item],
    selectedId: item.id,
    itemRef: item,
    payloadKind: "single-approve",
    rawFeedback: rawFeedback,
    payloadFeedback: payloadFeedback,
    feedbackDraftsById: feedbackDraftsById,
    expandedItemIds: opts.expandedItemIds || [],
    voiceTarget: opts.voiceTarget || "message-agent",
    imageAction: imageAction,
  });
};

// Self-init: read `traces.enabled` from `/__settings` once at iframe
// load and cache the result on `window.__bramTracesEnabled`. The
// `iframeTrace` (above) and `logToHost` (above) bodies gate on
// this flag so trace-off sessions skip the IPC roundtrip entirely
// instead of paying the cost only for the host to drop the line.
// Default-ON until the fetch resolves preserves current behavior
// during the ~50 ms startup window. Iframe-reload re-runs this on
// every settings change (existing watcher pattern), so live
// reactivity isn't needed here.
(function loadTracesEnabledFlag() {
  if (typeof window === "undefined") return;
  if (window.__bramTracesEnabled !== undefined) return;
  window.__bramTracesEnabled = true;
  if (typeof fetch !== "function") return;
  fetch("/__settings")
    .then(function (r) { return r && r.ok ? r.json() : null; })
    .then(function (s) {
      if (s && s.traces && typeof s.traces.enabled === "boolean") {
        window.__bramTracesEnabled = s.traces.enabled;
      }
    })
    .catch(function () {});
})();

// Interleave devtools console output + unhandled-error paths into
// bram-trace.log via the iframe-trace channel. Catches what previously
// only landed in the browser devtools panel (e.g. the toolbar
// __toolbarPendingMenuPresent scope errors fixed in 4ad0716). Inherits
// the master-flag short-circuit via the gate in `logToHost` above.
//
// Uses window.logToHost directly rather than `window.iframeTrace`
// above; payload shape is the same (kind="iframe-trace", subkind=...)
// but the explicit logToHost call sidesteps a re-entrancy risk if
// iframeTrace ever logged a console error.
(function installConsoleInterleave() {
  if (typeof window.logToHost !== "function") return;
  if (window.__bramConsoleInterleaveInstalled) return;
  window.__bramConsoleInterleaveInstalled = true;

  var inTrace = false;
  function safeStringify(a) {
    try {
      return typeof a === "string" ? a : JSON.stringify(a);
    } catch (e) {
      return String(a);
    }
  }
  function consoleArgDetail(a) {
    var isError = a && (a instanceof Error || a.stack || a.message);
    if (isError) {
      return {
        type: (a && a.name) || "Error",
        message: String((a && a.message) || a),
        stack: a && a.stack ? String(a.stack) : "",
      };
    }
    return {
      type: typeof a,
      preview: safeStringify(a),
    };
  }
  function consoleArgDetails(args) {
    return args.map(consoleArgDetail);
  }
  function firstConsoleStack(args) {
    for (var i = 0; i < args.length; i += 1) {
      if (args[i] && args[i].stack) return String(args[i].stack);
    }
    return "";
  }
  function runtimeErrorFields(message, source, lineno, colno, error, via) {
    return {
      message: message || (error && error.message) || "window error",
      filename: source,
      lineno: lineno,
      colno: colno,
      errorName: error && error.name,
      errorMessage: error && error.message,
      stack: error && error.stack,
      source: via,
    };
  }
  function emit(subkind, fields) {
    if (inTrace) return;
    inTrace = true;
    try {
      var payload = {
        kind: "iframe-trace",
        subkind: subkind,
        at: new Date().toISOString(),
      };
      Object.keys(fields || {}).forEach(function (k) {
        if (fields[k] !== undefined) payload[k] = fields[k];
      });
      window.logToHost(payload);
    } catch (_) {}
    inTrace = false;
  }

  ["log", "warn", "error"].forEach(function (level) {
    var orig = console[level];
    console[level] = function () {
      var args = Array.prototype.slice.call(arguments);
      emit("console-" + level, {
        message: args.map(safeStringify).join(" "),
        args: consoleArgDetails(args),
        stack: firstConsoleStack(args),
      });
      orig.apply(console, args);
    };
  });

  var previousOnError = window.onerror;
  window.onerror = function (message, source, lineno, colno, error) {
    emit("console-error", runtimeErrorFields(message, source, lineno, colno, error, "window.onerror"));
    if (typeof previousOnError === "function") {
      return previousOnError.apply(this, arguments);
    }
    return false;
  };

  window.addEventListener("error", function (e) {
    emit("console-error", runtimeErrorFields(
      e && e.message,
      e && e.filename,
      e && e.lineno,
      e && e.colno,
      e && e.error,
      "window.error"
    ));
  });

  window.addEventListener("unhandledrejection", function (e) {
    var reason = e && e.reason;
    emit("console-unhandledrejection", {
      message:
        (reason && (reason.message || String(reason))) || "unhandled rejection",
      stack: reason && reason.stack,
    });
  });
})();
// Setter for window.__bramMenuPending, called from Globals.xs
// applyAgentMenu. XMLUI's expression engine can't handle
// `window.__bramMenuPending = ...` as an assignment target (it parses
// the LHS as a bare variable and emits "Left value variable
// (__bramMenuPending) not found in the scope"), but function calls on
// window members evaluate fine. Bridging through this setter keeps
// the assignment in plain-JS scope.
window.__bramSetMenuPending = function (v) {
  window.__bramMenuPending = !!v;
};

// Plain-JS wrappers for the AgentMenu pty-menu-changed and
// turn-state-changed subscriber callbacks (registered in
// AgentMenu.xmlui onInit). XMLUI's expression engine runs subscriber
// arrow-function bodies through processStatementQueueAsync
// (xmlui/src/components-core/script-runner/process-statement-async.ts:115-166),
// which `await`s three times per statement — onStatementStarted,
// processStatementAsync, onStatementCompleted. Under iframe load
// each await is a microtask boundary that yields to the event
// loop, queueing the body behind pending macrotasks (DataSource
// polls, ChangeListener fires, JSONL broadcasts). End-to-end:
// 2-3 s between subscriber-fired (callback wrapper returns in 0 ms)
// and listener-fired (the iframeTrace inside setAgentMenuFromEvent
// actually emits). Collapsing the body to one window function call
// keeps applyAgentMenu, agentMenuTraceFields, iframeTrace, and the
// menu-pending mirror all on the synchronous JS side so the entire
// chain is one XMLUI statement instead of N.
// Native plain-JS AgentMenu state + handlers. Source of truth lives
// on window so xs scope can read it (Globals.xs getAgentMenu,
// Main.xmlui suppression gates) and JS scope can write it without
// going through XMLUI's expression engine.
//
// XMLUI evaluates xs function bodies via processStatementQueueAsync,
// awaiting three times per statement
// (xmlui/src/components-core/script-runner/process-statement-async.ts:115-166).
// Under iframe load — DataSource polls, ChangeListener fires, JSONL
// pipeline — each await yields to the event loop and the body
// serialises behind pending macrotasks. The full menu-state update
// (apply + trace) used to take 2-3 s end-to-end despite the JS-level
// subscriber wrapper returning in 0 ms. Doing the work natively
// here, before the XMLUI subscriber runs, drops that to the IPC
// delivery floor.
if (typeof window.bramAgentMenu === "undefined") window.bramAgentMenu = null;
if (typeof window.bramAgentMenuSuppressFallback === "undefined") window.bramAgentMenuSuppressFallback = true;
if (typeof window.bramAgentMenuLastHostMs === "undefined") window.bramAgentMenuLastHostMs = 0;
if (typeof window.bramAgentMenuLastSource === "undefined") window.bramAgentMenuLastSource = "";

function __bramAgentMenuHostMs(menu) {
  return menu && typeof menu.atHostMs === "number" ? menu.atHostMs : 0;
}

function __bramAgentMenuTraceFields(menu) {
  var hostMs = __bramAgentMenuHostMs(menu);
  return {
    tool: (menu && menu.tool) || "",
    hasSignature: !!(menu && menu.toolCallSignature),
    signatureChars: menu && menu.toolCallSignature ? menu.toolCallSignature.length : 0,
    assignedMenu: window.bramAgentMenu ? window.bramAgentMenu.tool : "",
    suppressFallback: window.bramAgentMenuSuppressFallback,
    at_host_ms: hostMs,
    delta_to_emit_ms: hostMs ? (Date.now() - hostMs) : -1,
    cache_source: (menu && menu.cacheSource) || "",
    last_host_ms: window.bramAgentMenuLastHostMs,
    last_cache_source: window.bramAgentMenuLastSource,
    stale: hostMs && window.bramAgentMenuLastHostMs && hostMs < window.bramAgentMenuLastHostMs ? 1 : 0,
  };
}

function __bramEmitMenuTrace(subkind, fields) {
  if (typeof window.logToHost !== "function") return;
  var payload = { kind: "iframe-trace", subkind: subkind, at: new Date().toISOString() };
  Object.keys(fields || {}).forEach(function (k) {
    if (fields[k] !== undefined) payload[k] = fields[k];
  });
  window.logToHost(payload);
}

window.__bramApplyAgentMenu = function (menu, suppressFallback, source) {
  var hostMs = __bramAgentMenuHostMs(menu);
  var stale = !!(hostMs && window.bramAgentMenuLastHostMs && hostMs < window.bramAgentMenuLastHostMs);
  if (stale) {
    __bramEmitMenuTrace("agent-menu-stale", {
      incoming_host_ms: hostMs,
      current_host_ms: window.bramAgentMenuLastHostMs,
      incoming_source: (menu && menu.cacheSource) || source || "",
      current_source: window.bramAgentMenuLastSource,
      incoming_tool: (menu && menu.tool) || "",
      current_tool: (window.bramAgentMenu && window.bramAgentMenu.tool) || "",
    });
    return true;
  }
  window.bramAgentMenu = menu || null;
  window.bramAgentMenuSuppressFallback = suppressFallback;
  window.__bramMenuPending = !!menu;
  if (hostMs) {
    window.bramAgentMenuLastHostMs = hostMs;
    window.bramAgentMenuLastSource = (menu && menu.cacheSource) || source || "";
  } else if (!menu) {
    window.bramAgentMenuLastHostMs = Date.now();
    window.bramAgentMenuLastSource = source || "";
  }
  return false;
};

window.__bramSetAgentMenuFromEvent = function (e, surface) {
  var payload = e && e.payload ? e.payload : null;
  var incoming = payload && payload.tool ? payload : null;
  var stale = window.__bramApplyAgentMenu(incoming, !incoming, "setAgentMenuFromEvent");
  var fields = __bramAgentMenuTraceFields(incoming);
  fields.context = "pty-menu-changed";
  fields.surface = surface || "agent-menu";
  fields.stale = stale;
  __bramEmitMenuTrace("listener-fired", fields);
};

window.__bramSetAgentMenuFromTurnState = function (turnState, surface) {
  var p = turnState || {};
  var incoming = p.pendingMenu || null;
  var stale = window.__bramApplyAgentMenu(incoming, !incoming, "setAgentMenuFromTurnState");
  var fields = __bramAgentMenuTraceFields(incoming);
  fields.context = "turn-state-changed";
  fields.surface = surface || "agent-menu";
  fields.phase = p.phase || "";
  fields.source = p.source || "";
  fields.menu = p.pendingMenu ? p.pendingMenu.tool : "";
  fields.stale = stale;
  __bramEmitMenuTrace("listener-fired", fields);
};

// Native subscriber registration lives further down in this file
// (search "__bramNativePtyMenuUnsub"). subscribeTauriEvent is defined
// later than this block, so calling it here at top level throws and
// aborts the rest of the script — taking down voice helpers, the
// console-interleave, and the Tauri-listener machinery itself
// (incident 2026-06-14: blank menus + voice broken). Register after
// subscribeTauriEvent exists.
window.openExternal = function (url) {
  var invoke = getTauriInvoke();
  if (!invoke) return;
  invoke("open_url", { url: String(url) }).catch(function (e) {
    console.error("openExternal open_url", e);
  });
};
// Capture an interactive screenshot via the host (macOS: screencapture -i)
// and inject the resulting file path into the terminal as a fresh user turn
// so claude reads it via its Read tool. User cancellation (Esc during the
// rect drag) is silent; other errors go to the host log.
window.captureScreenshot = function () {
  function deliver(path) {
    // Dual format: `@<path>` is claude-code's file-reference syntax (tells
    // the model to use its Read tool), and `[Image: source: <path>]` is
    // the marker Talk's extractImagePaths matches to render a thumbnail.
    // stripImagePaths removes the marker from the visible text, so the
    // displayed user turn shows "Read this screenshot: @path" plus the
    // inline thumbnail below.
    if (path) toTurn("Read this screenshot: @" + path + "\n[Image: source: " + path + "]");
  }
  function report(err) {
    var msg = String((err && err.message) || err);
    if (msg !== "cancelled") {
      logToHost({ kind: "screenshot", error: msg });
    }
  }
  var invoke = getTauriInvoke();
  if (!invoke) {
    report(new Error("Tauri IPC unavailable"));
    return;
  }
  invoke("capture_screenshot", {}).then(deliver).catch(report);
};

// Stage a clipboard-pasted image to disk via /__paste-image and remember its
// path so submitWorklistMessageFast can prepend the `[Image: source: <path>]`
// marker on the next form submit. Mirrors the marker protocol that
// captureScreenshot uses and that st_extract_image_paths reads back.
//
// We listen for paste events at document level so any Cmd/Ctrl+V — including
// one fired from the TextArea — stages clipboard images. The original
// FileUploadDropZone-based UX required clicking the dropzone first, but the
// underlying react-dropzone setup is configured with noKeyboard:true, which
// strips the rootDiv's tabIndex (react-dropzone/src/index.js:920); without
// focus the rootDiv never receives the React paste event, so click-then-paste
// silently no-ops. Window-level listening sidesteps the focus problem.
window.bramPendingPastedImages = window.bramPendingPastedImages || [];
window.bramStagingPastedImages = window.bramStagingPastedImages || 0;

// Paste-state pub/sub registry — bridge from helpers.js (canonical store) to
// XMLUI via the <External> component's `(emit) => unsubscribe` contract.
// helpers.js owns window.bramPendingPastedImages and
// window.bramStagingPastedImages above; every mutation site below calls
// bramNotifyPasteState() so the subscribers below re-snapshot and push the
// new value to their XMLUI-side observers. Replaces the 4 Hz <Timer> polling
// loop the strip used to do.
var bramPasteStateSubscribers = new Set();
function bramComputePasteState(target) {
  return {
    count: target
      ? window.bramPendingPastedImageCountForTarget(target)
      : window.bramPendingPastedImageCount(),
    paths: target
      ? window.bramPendingPastedImagePathsForTarget(target)
      : window.bramPendingPastedImagePaths(),
    staging: window.bramStagingPastedImageCount(),
  };
}
window.__bramPasteStateSnapshot = function (target) {
  return bramComputePasteState(target);
};
function bramNotifyPasteState() {
  bramPasteStateSubscribers.forEach(function (cb) {
    try { cb(); } catch (e) { console.error("[bram-paste] subscriber threw:", e); }
  });
}
// Memoize the per-target subscribe closure. XMLUI re-evaluates
// `subscribe="{window.bramSubscribePasteState(target)}"` on every render;
// returning a fresh closure each call makes the <External> useEffect's
// [subscribeFn] dep see a new identity each time, which kicks off a
// subscribe → emit → re-render → re-subscribe loop. Caching keyed on
// target gives every call with the same target the same function
// identity, so useEffect runs exactly once per real target change.
var bramSubscribePasteStateCache = Object.create(null);
window.bramSubscribePasteState = function (target) {
  var key = target == null ? "" : String(target);
  if (bramSubscribePasteStateCache[key]) return bramSubscribePasteStateCache[key];
  var cached = function (emit) {
    var fire = function () { emit(bramComputePasteState(target)); };
    bramPasteStateSubscribers.add(fire);
    fire();  // seed initial value synchronously
    return function () { bramPasteStateSubscribers.delete(fire); };
  };
  bramSubscribePasteStateCache[key] = cached;
  return cached;
};
window.bramActiveVoiceTargetMirror = window.bramActiveVoiceTargetMirror || "";
window.bramActiveFocusedFeedbackItemIdMirror = window.bramActiveFocusedFeedbackItemIdMirror || "";
window.bramSetActiveVoiceTargetMirror = function (v) {
  var prev = window.bramActiveVoiceTargetMirror || "";
  var next = v || "";
  window.bramActiveVoiceTargetMirror = next;
  if (window.__bramIframeTrace) window.__bramIframeTrace("paste-target-mirror", { kind: "voice", value: next, prev: prev });
};
window.bramSetActiveFocusedFeedbackItemIdMirror = function (v) {
  var prev = window.bramActiveFocusedFeedbackItemIdMirror || "";
  var next = v || "";
  window.bramActiveFocusedFeedbackItemIdMirror = next;
  if (window.__bramIframeTrace) window.__bramIframeTrace("paste-target-mirror", { kind: "focused-feedback-item", value: next, prev: prev });
};
window.bramCurrentPasteTarget = function () {
  var voice = window.bramActiveVoiceTargetMirror || "";
  var focusedFeedback = window.bramActiveFocusedFeedbackItemIdMirror || "";
  var active = document.activeElement;
  var placeholder = active && active.getAttribute && (active.getAttribute("placeholder") || "");
  var activeLooksLikeFeedback = placeholder === "Message to agent";
  var activeLooksLikeMessage = placeholder.indexOf("Message agent") === 0;
  var result;
  if (activeLooksLikeFeedback && focusedFeedback) {
    result = "feedback:" + focusedFeedback;
  } else if (activeLooksLikeMessage) {
    result = "message-agent";
  } else {
    result = voice;
  }
  if (window.__bramIframeTrace) window.__bramIframeTrace("paste-current-target", {
    voice: voice,
    focusedFeedback: focusedFeedback,
    placeholder: placeholder,
    activeLooksLikeFeedback: activeLooksLikeFeedback,
    activeLooksLikeMessage: activeLooksLikeMessage,
    result: result
  });
  return result;
};
window.bramPastedImageForCurrentTurn = window.bramPastedImageForCurrentTurn || false;
window.bramPastedImageTarget = window.bramPastedImageTarget || "";
window.bramLastConsumedPastedImages = window.bramLastConsumedPastedImages || [];
window.bramPasteImageTraceSigs = window.bramPasteImageTraceSigs || {};
function bramPendingPastedImageSummary() {
  return (window.bramPendingPastedImages || []).map(function (e) {
    if (typeof e === "string") return { path: e, target: "" };
    return { path: (e && e.path) || "", target: (e && e.target) || "" };
  }).filter(function (e) { return !!e.path; });
}
function bramActiveElementSummary() {
  var el = document.activeElement;
  if (!el) return "";
  var bits = [];
  if (el.tagName) bits.push(String(el.tagName).toLowerCase());
  if (el.id) bits.push("#" + el.id);
  var aria = el.getAttribute && (el.getAttribute("aria-label") || el.getAttribute("placeholder"));
  if (aria) bits.push("[" + String(aria).slice(0, 40) + "]");
  return bits.join("");
}
function bramTracePasteImage(stage, payload, sampleKey) {
  try {
    var p = Object.assign({ stage: stage }, payload || {});
    if (sampleKey) {
      var sig = JSON.stringify(p);
      if (window.bramPasteImageTraceSigs[sampleKey] === sig) return;
      window.bramPasteImageTraceSigs[sampleKey] = sig;
    }
    if (typeof window.__bramIframeTrace === "function") {
      window.__bramIframeTrace("paste-image", p);
    }
  } catch (e) {}
}
document.addEventListener("paste", function (event) {
  if (!event.clipboardData) return;
  var items = event.clipboardData.items || [];
  var imageFiles = [];
  for (var i = 0; i < items.length; i++) {
    var item = items[i];
    if (item.kind === "file" && /^image\//.test(item.type || "")) {
      var f = item.getAsFile();
      if (f) imageFiles.push(f);
    }
  }
  if (imageFiles.length === 0) return;
  // Accumulate pasted images across paste events within a single turn.
  // Originally (804bc37) this point cleared `bramPendingPastedImages`
  // on every paste to avoid sticking on stale images from abandoned
  // drafts, but the clear made multi-paste-event accumulation
  // impossible — pasting four screenshots one after another into a
  // single Iterate feedback box dropped all but one (race-dependent
  // first or last). Staleness is now handled by
  // `bramConsumePastedImagePaths` on turn submission and by the
  // `bramPastedImageForCurrentTurn` flag below.
  window.bramPastedImageForCurrentTurn = true;
  var currentTarget = (window.bramCurrentPasteTarget && window.bramCurrentPasteTarget()) || "";
  var pasteTarget = currentTarget || "message-agent";
  window.bramPastedImageTarget = pasteTarget;
  bramTracePasteImage("intake", {
    source: "paste",
    currentTarget: currentTarget,
    target: pasteTarget,
    activeElement: bramActiveElementSummary(),
    fileCount: imageFiles.length,
    pendingBefore: bramPendingPastedImageSummary()
  });
  // Suppress the default paste so the TextArea doesn't pick up any file-path
  // or filename text the OS may have placed on the clipboard alongside the
  // image (Finder copy-image, macOS screenshot tool, etc.).
  event.preventDefault();
  for (var j = 0; j < imageFiles.length; j++) {
    window.bramStagePastedImage(imageFiles[j], pasteTarget);
  }
});
// Drag-and-drop image intake — parallels the paste handler above.
function bramImageFilesFromDataTransfer(dt) {
  if (!dt) return [];
  var imageFiles = [];
  var items = dt.items || [];
  for (var i = 0; i < items.length; i++) {
    var item = items[i];
    if (item.kind === "file" && /^image\//.test(item.type || "")) {
      var f = item.getAsFile();
      if (f) imageFiles.push(f);
    }
  }
  if (imageFiles.length > 0) return imageFiles;
  var files = dt.files || [];
  for (var j = 0; j < files.length; j++) {
    var file = files[j];
    if (file && /^image\//.test(file.type || "")) imageFiles.push(file);
  }
  return imageFiles;
}
document.addEventListener("dragover", function (event) {
  if (bramImageFilesFromDataTransfer(event.dataTransfer).length === 0) return;
  event.preventDefault();
  if (event.dataTransfer) event.dataTransfer.dropEffect = "copy";
});
document.addEventListener("drop", function (event) {
  var imageFiles = bramImageFilesFromDataTransfer(event.dataTransfer);
  if (imageFiles.length === 0) return;
  window.bramPastedImageForCurrentTurn = true;
  var currentTarget = (window.bramCurrentPasteTarget && window.bramCurrentPasteTarget()) || "";
  var dropTarget = currentTarget || "message-agent";
  window.bramPastedImageTarget = dropTarget;
  bramTracePasteImage("intake", {
    source: "drop",
    currentTarget: currentTarget,
    target: dropTarget,
    activeElement: bramActiveElementSummary(),
    fileCount: imageFiles.length,
    pendingBefore: bramPendingPastedImageSummary()
  });
  event.preventDefault();
  for (var i = 0; i < imageFiles.length; i++) {
    window.bramStagePastedImage(imageFiles[i], dropTarget);
  }
});
window.bramStagePastedImage = function (file, target) {
  if (!file) return Promise.reject(new Error("no file"));
  var type = file.type || "image/png";
  var url = "/__paste-image?type=" + encodeURIComponent(type);
  var stageTarget = target || window.bramPastedImageTarget || "message-agent";
  // Read as ArrayBuffer first. `fetch(url, { body: file })` with a File body
  // in this Tauri webview wrote 0-byte files server-side (the host saw an
  // empty request body). Sending an ArrayBuffer via fetch reliably carries
  // the bytes through.
  return new Promise(function (resolve, reject) {
    var reader = new FileReader();
    window.bramStagingPastedImages++;
    bramNotifyPasteState();
    bramTracePasteImage("stage-start", { target: stageTarget, type: type, staging: window.bramStagingPastedImages });
    reader.onload = function () {
      if (!reader.result || reader.result.byteLength === 0) {
        var empty = new Error("paste-image: empty clipboard image");
        bramTracePasteImage("empty", { target: stageTarget });
        window.bramStagingPastedImages = Math.max(0, (window.bramStagingPastedImages || 0) - 1);
        bramNotifyPasteState();
        reject(empty);
        return;
      }
      fetch(url, {
        method: "POST",
        body: reader.result,
        headers: { "Content-Type": type },
      })
        .then(function (r) {
          if (!r.ok) throw new Error("paste-image HTTP " + r.status);
          return r.json();
        })
        .then(function (json) {
          if (!json || !json.path) throw new Error("paste-image: no path in response");
          var entry = { path: json.path, target: stageTarget };
          window.bramPendingPastedImages.push(entry);
          bramNotifyPasteState();
          bramTracePasteImage("staged", {
            path: json.path,
            target: stageTarget,
            currentGlobalTarget: window.bramPastedImageTarget || "",
            bytes: reader.result.byteLength,
            pendingAfter: bramPendingPastedImageSummary()
          });
          resolve(json.path);
        })
        .catch(function (e) {
          bramTracePasteImage("error", { target: stageTarget, message: String((e && e.message) || e) });
          reject(e);
        })
        .finally(function () {
          window.bramStagingPastedImages = Math.max(0, (window.bramStagingPastedImages || 0) - 1);
          bramNotifyPasteState();
        });
    };
    reader.onerror = function () {
      bramTracePasteImage("read-error", { target: stageTarget, message: String(reader.error || "") });
      window.bramStagingPastedImages = Math.max(0, (window.bramStagingPastedImages || 0) - 1);
      bramNotifyPasteState();
      reject(reader.error);
    };
    reader.readAsArrayBuffer(file);
  });
};
window.bramConsumePastedImagePaths = function (target) {
  if (!window.bramPastedImageForCurrentTurn) {
    window.bramPendingPastedImages = [];
    window.bramPastedImageForCurrentTurn = false;
    window.bramPastedImageTarget = "";
    window.bramLastConsumedPastedImages = [];
    bramTracePasteImage("consume", { target: target || "", reason: "no-current-turn", consumed: [], retained: [] });
    bramNotifyPasteState();
    return [];
  }
  var arr = window.bramPendingPastedImages || [];
  if (!target) {
    var allPaths = arr.map(function (e) { return e && e.path; }).filter(Boolean);
    window.bramPendingPastedImages = [];
    window.bramPastedImageForCurrentTurn = false;
    window.bramPastedImageTarget = "";
    window.bramLastConsumedPastedImages = allPaths.slice();
    bramTracePasteImage("consume", { target: "", mode: "drain-all", consumed: allPaths, retained: [] });
    bramNotifyPasteState();
    return allPaths;
  }
  var kept = [];
  var taken = [];
  for (var i = 0; i < arr.length; i++) {
    var e = arr[i];
    if (e && (e.target || "") === target) {
      if (e.path) taken.push(e.path);
    } else if (e) {
      kept.push(e);
    }
  }
  window.bramPendingPastedImages = kept;
  if (kept.length === 0) {
    window.bramPastedImageForCurrentTurn = false;
    window.bramPastedImageTarget = "";
  }
  window.bramLastConsumedPastedImages = taken.slice();
  bramTracePasteImage("consume", {
    target: target,
    mode: "target",
    consumed: taken,
    retained: bramPendingPastedImageSummary()
  });
  bramNotifyPasteState();
  return taken;
};
window.bramLastConsumedPastedImagePaths = function () {
  return (window.bramLastConsumedPastedImages || []).slice();
};
window.bramRemovePastedImagePath = function (path) {
  if (!path) return;
  var arr = window.bramPendingPastedImages || [];
  for (var i = 0; i < arr.length; i++) {
    var e = arr[i];
    if (e && e.path === path) {
      arr.splice(i, 1);
      bramTracePasteImage("removed", { path: path, target: e.target || "", pendingAfter: bramPendingPastedImageSummary() });
      bramNotifyPasteState();
      return;
    }
  }
};
window.bramHasPendingPastedImages = function () {
  return (window.bramPendingPastedImages || []).length > 0;
};
window.bramPendingPastedImageCount = function () {
  return (window.bramPendingPastedImages || []).length;
};
window.bramPendingPastedImageCountForTarget = function (target) {
  var t = target || "";
  var count = (window.bramPendingPastedImages || []).filter(function (e) {
    return e && (e.target || "") === t;
  }).length;
  bramTracePasteImage("query-count", { target: t, count: count }, "count:" + t);
  return count;
};
window.bramPendingPastedImagePaths = function () {
  return (window.bramPendingPastedImages || []).map(function (e) { return e && e.path; }).filter(Boolean);
};
window.bramPendingPastedImagePathsForTarget = function (target) {
  var t = target || "";
  var paths = (window.bramPendingPastedImages || [])
    .filter(function (e) { return e && (e.target || "") === t; })
    .map(function (e) { return e.path; })
    .filter(Boolean);
  bramTracePasteImage("query-paths", { target: t, count: paths.length, paths: paths }, "paths:" + t);
  return paths;
};
window.bramTracePastedImageStrip = function (source, target, count, paths, staging) {
  bramTracePasteImage("strip", {
    source: source || "",
    target: target || "",
    count: count || 0,
    paths: paths || [],
    staging: staging || 0
  }, "strip:" + (source || "") + ":" + (target || ""));
};
window.bramStagingPastedImageCount = function () {
  return window.bramStagingPastedImages || 0;
};

// Click-to-toggle voice. Single in-flight session per iframe.
//   voiceStart()              — starts recording (parent records on iframe's behalf).
//   voiceStop(callback)       — stops; callback(transcript) fires when transcript is ready.
// XMLUI's onClick expression evaluator does not reliably execute .then() callbacks
// attached during expression evaluation; passing a callback function as an argument
// works, since the callback is invoked from plain JS later.
window._voiceSession = null;
window._voiceStartedListener = null;
function _voiceLog(stage, payload) {
  try {
    window.logToHost(
      Object.assign(
        { kind: "voice", stage: stage, at: new Date().toISOString() },
        payload || {},
      ),
    );
  } catch (e) {}
}
function _voiceRemoveStartedListener() {
  if (window._voiceStartedListener) {
    try {
      window.removeEventListener("message", window._voiceStartedListener);
    } catch (e) {}
    window._voiceStartedListener = null;
  }
}
window.voiceStart = function (onStarted, onFailed) {
  if (window._voiceSession) {
    _voiceLog("voiceStart-rejected-already-active", {
      currentSession: window._voiceSession,
    });
    return;
  }
  _voiceRemoveStartedListener();
  var requestId =
    "voice-" + Date.now() + "-" + Math.random().toString(36).slice(2);
  window._voiceSession = requestId;
  _voiceLog("voiceStart", { requestId: requestId });
  function onStartedMsg(ev) {
    var data = ev && ev.data;
    if (!data || (data.type !== "voice-recording-started" && data.type !== "voice-into-result")) return;
    if (data.requestId !== requestId) return;
    window.removeEventListener("message", onStartedMsg);
    if (window._voiceStartedListener === onStartedMsg) {
      window._voiceStartedListener = null;
    }
    if (data.type === "voice-into-result") {
      if (window._voiceSession === requestId) {
        window._voiceSession = null;
      }
      _voiceLog("voiceStart-rejected-by-parent", {
        requestId: requestId,
        reason: data.reason || "",
        activeWas: data.activeWas || "",
        activeRequestId: data.activeRequestId || "",
        transcriptLength: String(data.transcript || "").length,
      });
      if (typeof onFailed === "function") {
        try { onFailed(data); } catch (e) {}
      }
      return;
    }
    if (window._voiceSession !== requestId) {
      _voiceLog("voice-recording-started-stale", { requestId: requestId });
      return;
    }
    _voiceLog("voice-recording-started", { requestId: requestId });
    if (typeof onStarted === "function") {
      try { onStarted(); } catch (e) {}
    }
  }
  window._voiceStartedListener = onStartedMsg;
  window.addEventListener("message", onStartedMsg);
  window.parent.postMessage(
    { type: "right-pane", kind: "voice-start", requestId: requestId },
    "*",
  );
};
window.voiceStop = function (callback) {
  var requestId = window._voiceSession;
  var stopAtMs = Date.now();
  window._voiceSession = null;
  _voiceRemoveStartedListener();
  if (!requestId) {
    _voiceLog("voiceStop-no-session", { stopAtMs: stopAtMs });
    if (typeof callback === "function") callback("");
    return;
  }
  _voiceLog("voiceStop", { requestId: requestId, stopAtMs: stopAtMs });
  function onResult(ev) {
    var data = ev && ev.data;
    if (!data || data.type !== "voice-into-result") return;
    var resultAtMs = Date.now();
    if (data.requestId !== requestId) {
      _voiceLog("voice-into-result-mismatch", {
        expected: requestId,
        received: data.requestId,
        stopAtMs: stopAtMs,
        stopToResultMs: resultAtMs - stopAtMs,
        transcriptPreview: String(data.transcript || "").slice(0, 80),
      });
      return;
    }
    window.removeEventListener("message", onResult);
    var transcript = String(data.transcript || "");
    var resultStopAtMs = Number(data.stopAtMs || stopAtMs);
    var voiceMeta = {
      requestId: requestId,
      stopAtMs: resultStopAtMs,
      stopToResultMs: resultAtMs - resultStopAtMs,
      parentStopToDeliverMs:
        typeof data.stopToDeliverMs === "number" ? data.stopToDeliverMs : null,
    };
    _voiceLog("voice-into-result", {
      requestId: requestId,
      stopAtMs: resultStopAtMs,
      stopToResultMs: voiceMeta.stopToResultMs,
      parentStopToDeliverMs: voiceMeta.parentStopToDeliverMs,
      transcriptLength: transcript.length,
      transcriptPreview: transcript.slice(0, 80),
    });
    if (typeof callback === "function") callback(transcript, voiceMeta);
  }
  window.addEventListener("message", onResult);
  window.parent.postMessage(
    { type: "right-pane", kind: "voice-stop", requestId: requestId, stopAtMs: stopAtMs },
    "*",
  );
};
// Snapshot of the iframe's current pixel size. Same-origin iframes can
// read their own viewport dimensions directly — no parent round-trip
// needed. Callback receives { width, height } as integers (rounded).
window.getRightPaneSize = function (callback) {
  if (typeof callback !== "function") return;
  callback({
    width: Math.round(window.innerWidth || 0),
    height: Math.round(window.innerHeight || 0),
  });
};

// Subscribe to session-JSONL change events. The parent shell receives
// `talk-session-changed` Tauri events from the file watcher; same-origin
// iframes consume them through this bridge. Used by Transcript / Workspace
// to refetch immediately on provider session-file writes — eliminates the
// poll-window lag where short-lived menu or turn-boundary state could come
// and go between ticks.
var __talkSessionSubscribers = [];
var __talkSessionMainUnsub = null;
window.onTalkSessionChange = function (fn) {
  if (typeof __talkSessionMainUnsub === "function") {
    try { __talkSessionMainUnsub(); } catch (e) {}
    __talkSessionMainUnsub = null;
  }
  if (typeof fn !== "function") return function () {};
  __talkSessionMainUnsub = window.subscribeTalkSessionChange("__bramMainTalkSessionUnsub", fn);
  return __talkSessionMainUnsub;
};
window.subscribeTalkSessionChange = function (key, fn) {
  if (typeof window[key] === "function") {
    try { window[key](); } catch (e) {}
  }
  if (typeof fn !== "function") {
    window[key] = null;
    return function () {};
  }
  __talkSessionSubscribers.push(fn);
  // Subscriber-lifecycle trace for the talk-session event-drop
  // investigation (#tsc-drop): a sub/resub churn pattern would explain
  // some of the 175→83 delivery gap if the parent listen() were
  // racing the iframe's swap window.
  try {
    if (typeof window.logToHost === "function") {
      window.logToHost({
        kind: "iframe-trace",
        subkind: "subscriber-changed",
        at: new Date().toISOString(),
        context: "talk-session-changed",
        op: "subscribe",
        key: key,
        count: __talkSessionSubscribers.length,
      });
    }
  } catch (e) {}
  window[key] = function () {
    var idx = __talkSessionSubscribers.indexOf(fn);
    if (idx >= 0) __talkSessionSubscribers.splice(idx, 1);
    try {
      if (typeof window.logToHost === "function") {
        window.logToHost({
          kind: "iframe-trace",
          subkind: "subscriber-changed",
          at: new Date().toISOString(),
          context: "talk-session-changed",
          op: "unsubscribe",
          key: key,
          count: __talkSessionSubscribers.length,
        });
      }
    } catch (e) {}
    window[key] = null;
  };
  return window[key];
};
// Cascade-diagnosis instrumentation (refs #93). Counts every
// talk-session-changed delivery and emits a rolling batch record
// every 10 events so we can see per-event cost + frequency without
// flooding bram-trace.
var __tscBatch = { count: 0, totalMs: 0, maxMs: 0, sinceMs: 0 };
function __tscBatchTick(elapsedMs) {
  if (__tscBatch.count === 0) __tscBatch.sinceMs = Date.now();
  __tscBatch.count += 1;
  __tscBatch.totalMs += elapsedMs;
  if (elapsedMs > __tscBatch.maxMs) __tscBatch.maxMs = elapsedMs;
  if (__tscBatch.count >= 10) {
    try {
      if (typeof window.logToHost === "function" && !window.__bramMenuPending) {
        window.logToHost({
          kind: "iframe-trace",
          subkind: "talk-session-batch",
          at: new Date().toISOString(),
          count: __tscBatch.count,
          sumMs: Math.round(__tscBatch.totalMs * 10) / 10,
          avgMs: Math.round((__tscBatch.totalMs / __tscBatch.count) * 10) / 10,
          maxMs: Math.round(__tscBatch.maxMs * 10) / 10,
          spanMs: Date.now() - __tscBatch.sinceMs,
        });
      }
    } catch (e) {}
    __tscBatch = { count: 0, totalMs: 0, maxMs: 0, sinceMs: 0 };
  }
}
// Parent-window-scoped Tauri-listener dedup, fixing the iframe-reload
// accumulation leak.
//
// Both ev.listen() call sites in this file (the direct
// talk-session-changed listener below and the dynamic one inside
// __ensureTauriEventListener) register on `window.parent.__TAURI__.event`,
// which lives on the parent shell webview and PERSISTS across iframe
// reloads. The iframe's own module-level state
// (__tauriEventListening / __tauriEventSubscribers) re-initialises on
// every load, so each fresh load thought no listener existed and
// registered another one — old closures from prior loads stayed live
// on the parent registry. One host emit then fanned out to N copies
// of every subscriber, multiplying refetch-called fires, debounce
// schedules, DataSource reloads, etc.
//
// Symptom we measured during the Globals.xs migration (commit d532432):
// listener-fired count per pty-menu-changed event grew from 4 → 5
// across two manual reloads of the same Bram session. Same pattern
// for talk-session-changed.
//
// Fix: keep a parent-window-scoped map of eventName → unsub function
// (or pending listen() promise). On each iframe load, drain the
// stale entry before calling ev.listen() again. Trace the drain so
// we can verify the dedup is firing.
function __bramListenWithDedup(ev, eventName, callback) {
  if (!ev || typeof ev.listen !== "function") return Promise.resolve(null);
  var parent;
  try {
    parent = (window.parent && window.parent !== window) ? window.parent : window;
  } catch (e) {
    parent = window;
  }
  try {
    if (!parent.__bramTauriListenerUnsubs) parent.__bramTauriListenerUnsubs = {};
  } catch (e) {}
  var store = null;
  try { store = parent.__bramTauriListenerUnsubs; } catch (e) {}
  // Dedup key must include iframe identity, not just eventName. Tools-pane
  // and right-pane both register Tauri listeners against the parent webview
  // (window.parent.__TAURI__.event), and each iframe's listener callback
  // closes over its OWN __tauriEventSubscribers array. Keying by eventName
  // alone made any later iframe's load drain the prior iframe's listener —
  // leaving the orphaned iframe's subscriber array (AgentMenu + Toolbar +
  // native, for the tools-pane) silently unwatched, so menus didn't render
  // on cold start until a manual reload made the affected iframe the last
  // to register. Same-iframe reloads still drain themselves (the original
  // 4→5 stale-listener bug from commit d532432 stays fixed).
  var iframeKey = (function () {
    try { return window.location.pathname || ""; } catch (e) { return ""; }
  })();
  var storeKey = eventName + "::" + iframeKey;
  var stale = store ? store[storeKey] : null;
  if (stale) {
    try {
      if (typeof stale === "function") {
        try { stale(); } catch (e) {}
      } else if (stale && typeof stale.then === "function") {
        stale.then(function (fn) { if (typeof fn === "function") { try { fn(); } catch (e) {} } }, function () {});
      }
    } catch (e) {}
    try { if (store) store[storeKey] = null; } catch (e) {}
    try {
      if (typeof window.logToHost === "function") {
        window.logToHost({
          kind: "iframe-trace",
          subkind: "tauri-listener-dedup",
          at: new Date().toISOString(),
          event_name: eventName,
          iframe_key: iframeKey,
          stage: "drained-stale",
        });
      }
    } catch (e) {}
  }
  var listenResult;
  try {
    listenResult = ev.listen(eventName, callback);
  } catch (e) {
    return Promise.resolve(null);
  }
  try { if (store) store[storeKey] = listenResult; } catch (e) {}
  Promise.resolve(listenResult).then(function (unsub) {
    try { if (store) store[storeKey] = unsub; } catch (e) {}
  }, function () {});
  return Promise.resolve(listenResult);
}
try {
  if (window.parent && window.parent.__TAURI__ && window.parent.__TAURI__.event) {
    __bramListenWithDedup(window.parent.__TAURI__.event, "talk-session-changed", function (event) {
      var t0 = (typeof performance !== "undefined" && performance.now) ? performance.now() : Date.now();
      // Per-emit correlation id from the host (see Rust
      // emit_talk_session_changed). Logged here so the trace records
      // the parent→iframe hand-off independently of any subscriber's
      // own listener-fired trace. at_host_ms lets each iframe-side
      // trace report delta_to_emit_ms — host emit → this point and,
      // via subscriber forwarding, host emit → listener-fired and
      // host emit → refetch-called.
      var correlationId = (event && event.payload && event.payload.correlation_id) || "";
      var atHostMs = (event && event.payload && typeof event.payload.at_host_ms === "number") ? event.payload.at_host_ms : 0;
      try {
        if (typeof window.logToHost === "function") {
          window.logToHost({
            kind: "iframe-trace",
            subkind: "event-received",
            at: new Date().toISOString(),
            context: "talk-session-changed",
            correlation_id: correlationId,
            subscribers: __talkSessionSubscribers.length,
            at_host_ms: atHostMs,
            delta_to_emit_ms: atHostMs ? (Date.now() - atHostMs) : -1,
          });
        }
      } catch (e) {}
      var n = __talkSessionSubscribers.length;
      for (var i = 0; i < n; i++) {
        try { __talkSessionSubscribers[i](correlationId, atHostMs); } catch (e) {}
      }
      var t1 = (typeof performance !== "undefined" && performance.now) ? performance.now() : Date.now();
      __tscBatchTick(t1 - t0);
    });
  }
} catch (e) {}

// Generic keyed-slot subscription to a parent-shell Tauri event (#81).
// Mirrors subscribeTalkSessionChange so the same leak fix applies to
// any event name: ONE parent listener per eventName, registered lazily
// on first subscribe and guarded so it attaches exactly once per
// helpers.js load, fanning out to a synchronous subscriber array. The
// per-mount subscribe call is fully synchronous (revoke window[key],
// push, store unsub) — no tauri.event.listen Promise window — so a
// component's onInit re-running on hot-reload keeps the live-subscriber
// count at exactly one. The prior direct tauri.event.listen(...).then()
// blocks stacked one live listener per onInit re-run.
var __tauriEventSubscribers = {};
var __tauriEventListening = {};
var __tauriEventListenReady = {};
function __ensureTauriEventListener(eventName) {
  if (__tauriEventListening[eventName]) return __tauriEventListenReady[eventName] || Promise.resolve(true);
  var ev = (window.parent && window.parent.__TAURI__ && window.parent.__TAURI__.event)
    || (window.__TAURI__ && window.__TAURI__.event);
  if (!ev || typeof ev.listen !== "function") return Promise.resolve(false);
  __tauriEventListening[eventName] = true;
  try {
    var listenResult = __bramListenWithDedup(ev, eventName, function (e) {
      var subs = __tauriEventSubscribers[eventName] || [];
      try {
        if (typeof window.logToHost === "function") {
          window.logToHost({
            kind: "iframe-trace",
            subkind: "event-received",
            at: new Date().toISOString(),
            event_name: eventName,
            subscribers: subs.length,
          });
        }
      } catch (err) {}
      for (var i = 0; i < subs.length; i++) {
        var subStart = (typeof performance !== "undefined" && performance.now)
          ? performance.now()
          : Date.now();
        try { subs[i](e); } catch (err) {}
        try {
          if (typeof window.logToHost === "function") {
            var subEnd = (typeof performance !== "undefined" && performance.now)
              ? performance.now()
              : Date.now();
            window.logToHost({
              kind: "iframe-trace",
              subkind: "subscriber-fired",
              at: new Date().toISOString(),
              event_name: eventName,
              subscriber_index: i,
              elapsed_ms: Math.round(subEnd - subStart),
            });
          }
        } catch (err) {}
      }
    });
    __tauriEventListenReady[eventName] = Promise.resolve(listenResult).then(
      function () { return true; },
      function () {
        __tauriEventListening[eventName] = false;
        return false;
      },
    );
  } catch (err) {
    __tauriEventListening[eventName] = false;
    __tauriEventListenReady[eventName] = Promise.resolve(false);
  }
  return __tauriEventListenReady[eventName];
}
function __notifyStartupReadyForEvent(eventName) {
  if (typeof window.fetch !== "function") return;
  window.fetch("/__startup-ready?event=" + encodeURIComponent(eventName), { cache: "no-store" })
    .then(function () {})
    .catch(function () {});
}
window.subscribeTauriEvent = function (key, eventName, fn) {
  if (typeof window[key] === "function") {
    try { window[key](); } catch (e) {}
  }
  if (typeof fn !== "function") {
    window[key] = null;
    return function () {};
  }
  if (!__tauriEventSubscribers[eventName]) __tauriEventSubscribers[eventName] = [];
  var listenReady = __ensureTauriEventListener(eventName);
  __tauriEventSubscribers[eventName].push(fn);
  window[key] = function () {
    var subs = __tauriEventSubscribers[eventName] || [];
    var idx = subs.indexOf(fn);
    if (idx >= 0) subs.splice(idx, 1);
    window[key] = null;
  };
  Promise.resolve(listenReady).then(function (ready) {
    if (!ready) return;
    var subs = __tauriEventSubscribers[eventName] || [];
    if (subs.indexOf(fn) >= 0) __notifyStartupReadyForEvent(eventName);
  });
  return window[key];
};

// Native plain-JS subscribers for the AgentMenu pipeline. Counterpart
// to window.__bramApplyAgentMenu / window.__bramSetAgentMenuFrom*
// defined earlier in this file. Registered here, AFTER
// window.subscribeTauriEvent exists, but BEFORE AgentMenu.xmlui's
// onInit calls subscribeTauriEvent with its trivial menuTick-bumping
// callback. Subscribers are dispatched by __ensureTauriEventListener
// in registration order, so the native handler updates
// window.bramAgentMenu in plain JS before the XMLUI subscriber's
// menuTick++ statement gets queued for evaluation. AgentMenu's
// `when={menuTick >= 0 && getAgentMenu(...)}` re-evaluation reads the
// already-updated window state.
window.subscribeTauriEvent("__bramNativePtyMenuUnsub", "pty-menu-changed", function (e) {
  window.__bramSetAgentMenuFromEvent(e, "agent-menu");
});
window.subscribeTauriEvent("__bramNativeTurnStateUnsub", "turn-state-changed", function (e) {
  window.__bramSetAgentMenuFromTurnState((e && e.payload) || {}, "agent-menu");
});

// Native subscribers for toolbar pending-menu state. Moved out of
// Main.xmlui's onInit blob (item: main-xmlui-tauri-subscribers-external).
// The arrow bodies that used to live in markup only called
// window.__bramSetToolbarPendingMenuFrom* — pure side-effects on
// window state, no App-level var dependencies. Same pattern as the
// AgentMenu native subscribers above.
window.subscribeTauriEvent("__bramNativeToolbarTurnStateUnsub",
  "turn-state-changed", function (e) {
    window.__bramSetToolbarPendingMenuFromTurnState((e && e.payload) || null);
  });
window.subscribeTauriEvent("__bramNativeToolbarPtyMenuUnsub",
  "pty-menu-changed", function (e) {
    window.__bramSetToolbarPendingMenuFromEvent(e);
  });

// External-driven agent-status bridge. Emits the agent-status-changed
// event payload; also performs the agent-header-status-loaded trace
// emit that used to live in Main.xmlui's onInit arrow body.
window.bramSubscribeAgentStatus = (function () {
  var factory;
  return function () {
    if (factory) return factory;
    var subscribers = new Set();
    var lastValue = null;
    var notify = function () {
      subscribers.forEach(function (fn) {
        try { fn(); } catch (e) { console.error("[bramSubscribeAgentStatus] subscriber threw:", e); }
      });
    };
    window.subscribeTauriEvent("__bramAgentStatusExternalUnsub",
      "agent-status-changed", function (e) {
        lastValue = (e && e.payload) || null;
        if (!window.bramAgentMenu) {
          window.__bramIframeTrace("agent-header-status-loaded", {
            state: (lastValue && lastValue.state) || "",
            verb: (lastValue && lastValue.verb) || "",
            provider: (lastValue && lastValue.provider) || "",
            source: (lastValue && lastValue.source) || "",
            elapsed: (lastValue && lastValue.elapsedText) || ""
          });
        }
        notify();
      });
    factory = function (emit) {
      var fire = function () { emit(lastValue); };
      subscribers.add(fire);
      fire();
      return function () { subscribers.delete(fire); };
    };
    return factory;
  };
})();

// External-driven conversation-state push. Replaces the conversationStateDS
// DataSource: the host emits `conversation-state-changed` with the full
// /__conversation-state payload, deduped, so subscribers re-render only on a
// real content change (not on every session signal). A one-time seed fetch
// populates the value on first subscribe so the chat pane is not blank before
// the first push.
window.bramSubscribeConversationState = (function () {
  var factory;
  return function () {
    if (factory) return factory;
    var subscribers = new Set();
    var lastValue = null;
    var notify = function () {
      subscribers.forEach(function (fn) {
        try { fn(); } catch (e) { console.error("[bramSubscribeConversationState] subscriber threw:", e); }
      });
    };
    window.subscribeTauriEvent("__bramConversationStateExternalUnsub",
      "conversation-state-changed", function (e) {
        lastValue = (e && e.payload) || null;
        notify();
      });
    // Seed once so the pane has content before the first push event arrives.
    try {
      window.fetch("/__conversation-state")
        .then(function (r) { return r.json(); })
        .then(function (v) { if (lastValue == null) { lastValue = v; notify(); } })
        .catch(function () {});
    } catch (e) {}
    factory = function (emit) {
      var fire = function () { emit(lastValue); };
      subscribers.add(fire);
      fire();
      return function () { subscribers.delete(fire); };
    };
    return factory;
  };
})();

// External-driven enhance-status tick. Emits an incrementing tick on
// each enhance-status-changed event so a downstream ChangeListener can
// trigger DataSource.refetch() (a markup-only operation).
window.bramSubscribeEnhanceStatusTick = (function () {
  var factory;
  return function () {
    if (factory) return factory;
    var subscribers = new Set();
    var tick = 0;
    window.subscribeTauriEvent("__bramEnhanceStatusExternalUnsub",
      "enhance-status-changed", function () {
        tick += 1;
        subscribers.forEach(function (fn) {
          try { fn(); } catch (e) { console.error("[bramSubscribeEnhanceStatusTick] subscriber threw:", e); }
        });
      });
    factory = function (emit) {
      var fire = function () { emit(tick); };
      subscribers.add(fire);
      fire();
      return function () { subscribers.delete(fire); };
    };
    return factory;
  };
})();

// Voice transcript scratch setter — invoked from xs arrow bodies that
// can't write `window.foo = x` as an LValue (XMLUI's expression engine
// rejects member-expression LValues with "Left value variable not
// found in scope" — see bram-trace 2026-06-17 00:43:03). Plain JS, no
// xs evaluator involvement.
// Plain-JS append helper. xs `function foo()` declarations do NOT
// reliably hoist onto window from the iframe's runtime context — see
// 2026-06-17 voice debugging where window.appendVoiceTranscript and
// window.bumpWorklistVoiceSeq calls returned without entering the
// function body. Defining the append helper directly on window
// guarantees the call lands.
window.__bramAppendVoiceToBox = function (component, transcript) {
  try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "windowAppend-enter", tLen: (transcript || "").length, hasComponent: !!component }); } catch (e) {}
  if (!component || !transcript) {
    try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "windowAppend-early-return", reason: !component ? "no-component" : "no-transcript" }); } catch (e) {}
    return false;
  }
  var current = String(component.value || "");
  var cleaned = transcript.replace(/\r?\n/g, " ").replace(/[ \t]+/g, " ").trim();
  if (!cleaned) {
    try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "windowAppend-cleaned-empty" }); } catch (e) {}
    return false;
  }
  var spacer = current && !/\s$/.test(current) ? " " : "";
  var next = current + spacer + cleaned;
  try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "windowAppend-calling-setValue", currentLen: current.length, nextLen: next.length }); } catch (e) {}
  try {
    component.setValue(next);
    try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "windowAppend-after-setValue" }); } catch (e) {}
  } catch (e) {
    try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "windowAppend-setValue-threw", error: String(e && e.message) }); } catch (e2) {}
    return false;
  }
  try {
    if (typeof component.focus === "function") component.focus();
    if (typeof component.setSelectionRange === "function") component.setSelectionRange(next.length, next.length);
  } catch (e) {}
  return next;
};

window.__bramSetLatestVoiceState = function (t, meta) {
  try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "setLatest-enter", tLen: (t || "").length }); } catch (e) {}
  window.__bramLatestVoiceTranscript = t || "";
  window.__bramLatestVoiceMeta = meta || null;
  try {
    window.dispatchEvent(new CustomEvent("bram:voice-arrival", {
      detail: { transcript: t || "", meta: meta || null, at: Date.now() },
    }));
    try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "setLatest-dispatched" }); } catch (e) {}
  } catch (e) {
    console.error("[bram] voice-arrival dispatch failed:", e);
  }
};

// External-driven voice-arrival bridge. xs-side writes to module vars
// (worklistVoiceSeq, worklistVoiceText) don't propagate through XMLUI's
// reactive system when triggered from arrow-body callbacks (see
// 2026-06-17 voice debugging). This External listens to a window-side
// CustomEvent that __bramSetLatestVoiceState dispatches, giving the
// XMLUI reactivity layer a path it can observe.
window.bramSubscribeVoiceArrival = (function () {
  var factory;
  return function () {
    if (factory) return factory;
    var subscribers = new Set();
    var lastEvent = null;
    var notify = function () {
      subscribers.forEach(function (fn) {
        try { fn(); } catch (e) { console.error("[bram] voice-arrival subscriber threw:", e); }
      });
    };
    window.addEventListener("bram:voice-arrival", function (evt) {
      lastEvent = (evt && evt.detail) || null;
      try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "external-event-received", tLen: ((lastEvent && lastEvent.transcript) || "").length, subscribers: subscribers.size }); } catch (e) {}
      notify();
    });
    factory = function (emit) {
      var fire = function () {
        try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "external-fire", hasEvent: !!lastEvent }); } catch (e) {}
        emit(lastEvent);
      };
      subscribers.add(fire);
      try { window.__bramIframeTrace && window.__bramIframeTrace("voice-trace", { stage: "external-subscribed", totalSubscribers: subscribers.size }); } catch (e) {}
      fire();
      return function () { subscribers.delete(fire); };
    };
    return factory;
  };
})();

// External-driven right-pane-size bridge. Same shape as the Tauri /
// agent-status / agent-menu factories, but the underlying source is
// the custom subscribeRightPaneSize API (window resize observer).
window.bramSubscribeRightPaneSize = (function () {
  var factory;
  return function () {
    if (factory) return factory;
    var subscribers = new Set();
    var lastSize = null;
    var notify = function () {
      subscribers.forEach(function (fn) {
        try { fn(); } catch (e) { console.error("[bramSubscribeRightPaneSize] subscriber threw:", e); }
      });
    };
    window.subscribeRightPaneSize(function (s) {
      lastSize = s || null;
      notify();
    });
    factory = function (emit) {
      var fire = function () { emit(lastSize); };
      subscribers.add(fire);
      fire();
      return function () { subscribers.delete(fire); };
    };
    return factory;
  };
})();

// External-driven talk-session-change bridge. Emits an event with
// the correlation id and host timestamp on each talk-session
// rotation.
window.bramSubscribeTalkSessionChange = (function () {
  var factory;
  return function () {
    if (factory) return factory;
    var subscribers = new Set();
    var lastEvent = null;
    var notify = function () {
      subscribers.forEach(function (fn) {
        try { fn(); } catch (e) { console.error("[bramSubscribeTalkSessionChange] subscriber threw:", e); }
      });
    };
    window.subscribeTalkSessionChange(
      "__bramTalkSessionExternalUnsub",
      function (correlationId, atHostMs) {
        lastEvent = {
          correlationId: correlationId || "",
          atHostMs: atHostMs || 0,
          at: Date.now(),
        };
        notify();
      }
    );
    factory = function (emit) {
      var fire = function () { emit(lastEvent); };
      subscribers.add(fire);
      fire();
      return function () { subscribers.delete(fire); };
    };
    return factory;
  };
})();

// Generic External-driven Tauri event factory. Memoizes per event
// name. Emits { tick, payload } on each fire — tick strictly
// increments to guarantee identity-change for listenTo expressions;
// payload carries the event data for consumers that need it.
window.bramSubscribeTauriEvent = (function () {
  var byEvent = Object.create(null);
  return function (eventName) {
    if (byEvent[eventName]) return byEvent[eventName];
    var subscribers = new Set();
    var tick = 0;
    var lastPayload = null;
    window.subscribeTauriEvent(
      "__bramTauriExternal_" + eventName,
      eventName,
      function (e) {
        tick += 1;
        lastPayload = (e && e.payload) || null;
        var snapshot = { tick: tick, payload: lastPayload };
        subscribers.forEach(function (fn) {
          try { fn(snapshot); } catch (err) {
            console.error("[bramSubscribeTauriEvent] subscriber threw:", err);
          }
        });
      }
    );
    var factory = function (emit) {
      var fire = function (snapshot) {
        emit(snapshot || { tick: tick, payload: lastPayload });
      };
      subscribers.add(fire);
      fire();
      return function () { subscribers.delete(fire); };
    };
    byEvent[eventName] = factory;
    return factory;
  };
})();

// External-driven AgentMenu bridge — emits the current pending menu
// when either Tauri event fires. Subscribes lazily on first call so
// the native subscribers above (registered at module load) are
// guaranteed to fire FIRST and update window.bramAgentMenu before
// compute() reads it.
window.bramSubscribeAgentMenu = (function () {
  var factory;
  return function () {
    if (factory) return factory;
    var lastTurnState = null;
    var subscribers = new Set();
    var compute = function () {
      var current = window.bramAgentMenu || null;
      var suppress = window.bramAgentMenuSuppressFallback !== false;
      return current ||
        (!suppress && lastTurnState && lastTurnState.pendingMenu) ||
        null;
    };
    var notify = function () {
      subscribers.forEach(function (fn) {
        try { fn(); } catch (e) { console.error("[bramSubscribeAgentMenu] subscriber threw:", e); }
      });
    };
    window.subscribeTauriEvent(
      "__bramAgentMenuExternalTurnUnsub",
      "turn-state-changed",
      function (e) { lastTurnState = (e && e.payload) || null; notify(); }
    );
    window.subscribeTauriEvent(
      "__bramAgentMenuExternalPtyUnsub",
      "pty-menu-changed",
      notify
    );
    factory = function (emit) {
      var fire = function () { emit(compute()); };
      subscribers.add(fire);
      fire();
      return function () { subscribers.delete(fire); };
    };
    return factory;
  };
})();

// Shared cache for the latest session-tail JSONL. A helper-side poller
// fetches /__sessions/latest-tail and calls setLatestJsonl() on each
// new value; both the Worklist tab (Workspace.xmlui) and the Transcript
// tab subscribe via onLatestJsonlChange() so they share one fetch and
// survive tab switches without losing the cached value. Keeping the
// fetch in helpers.js avoids routing large JSONL response bodies through
// XMLUI DataSource tracing / Inspector retention.
//
// Why a window-level cache and not global.lastJsonl on the App: XMLUI
// 0.12.27's global-write path runs the assigned value through its
// expression parser, and a JSONL string (starts with `{`) parses as
// the start of an unclosed XMLUI expression. Keeping the value in
// plain JS sidesteps the parser entirely.
var __latestJsonlValue = null;
var __latestJsonlSubscribers = [];
var __latestJsonlPollers = {};
window.getLatestJsonl = function () { return __latestJsonlValue; };
window.setLatestJsonl = function (value) {
  __latestJsonlValue = value;
  var n = __latestJsonlSubscribers.length;
  for (var i = 0; i < n; i++) {
    try { __latestJsonlSubscribers[i](value); } catch (e) {}
  }
  // Trace the broadcast so #100-style perf observation can see how many
  // subscribers were notified per fetch. With <Pages> only mounting one
  // route at a time, n is typically 1 after a single tab visit, 2 after
  // both tabs have been visited in this iframe session.
  try {
    if (window.logToHost && !window.__bramMenuPending) {
      window.logToHost({
        kind: "iframe-trace",
        subkind: "jsonl-broadcast",
        at: new Date().toISOString(),
        subscribers: n,
        len: (value && value.length) || 0,
      });
    }
  } catch (e) {}
};
window.onLatestJsonlChange = function (fn) {
  if (typeof fn !== "function") return function () {};
  __latestJsonlSubscribers.push(fn);
  return function () {
    var idx = __latestJsonlSubscribers.indexOf(fn);
    if (idx >= 0) __latestJsonlSubscribers.splice(idx, 1);
  };
};

// --- Transcript (foldable, structured, full history) ---------------------
// External subscribe factory for the latest session JSONL. Emits the
// current cached value on attach, then on every setLatestJsonl broadcast.
window.bramSubscribeLatestJsonl = function () {
  return function (emit) {
    try { emit(window.getLatestJsonl()); } catch (e) {}
    return window.onLatestJsonlChange(function (v) { emit(v); });
  };
};

// One-line summary for a tool card's collapsed state.
window.__bramTranscriptToolSummary = function (name, input) {
  input = input || {};
  if (name === "Bash") return input.command || "";
  if (name === "Read" || name === "Edit" || name === "Write" || name === "NotebookEdit") return input.file_path || "";
  if (name === "Grep" || name === "Glob") return input.pattern || "";
  if (input.description) return input.description;
  var keys = Object.keys(input);
  for (var k = 0; k < keys.length; k++) {
    if (typeof input[keys[k]] === "string") return input[keys[k]].slice(0, 200);
  }
  return "";
};

// Flatten a tool_result content (string | array-of-blocks) to text.
window.__bramTranscriptResultText = function (content) {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content.map(function (b) {
      return (b && b.type === "text") ? (b.text || "") : "";
    }).join("");
  }
  return "";
};

// Parse session JSONL into an ordered event stream:
//   { id, kind: 'user'|'text'|'thinking'|'tool', text?, name?, summary?,
//     input?, result?, isError? }
// The JSONL is already chronological (one content block per line), so this
// preserves the real play-by-play. tool_result lines are merged back into
// their originating tool event by tool_use_id.
window.__bramParseTranscript = function (jsonl) {
  if (!jsonl) return [];
  var lines = jsonl.split("\n");
  var events = [];
  var toolById = {};
  for (var i = 0; i < lines.length; i++) {
    var line = lines[i];
    if (!line) continue;
    var o;
    try { o = JSON.parse(line); } catch (e) { continue; }
    if (o.type !== "assistant" && o.type !== "user") continue;
    var msg = o.message || {};
    var role = msg.role;
    var content = msg.content;
    if (typeof content === "string") {
      if (content.trim()) {
        events.push({ id: "s" + i, kind: role === "user" ? "user" : "text", text: content });
      }
      continue;
    }
    if (!Array.isArray(content)) continue;
    for (var j = 0; j < content.length; j++) {
      var b = content[j];
      if (!b || typeof b !== "object") continue;
      var bid = i + "_" + j;
      if (b.type === "text") {
        if ((b.text || "").trim()) {
          events.push({ id: bid, kind: role === "user" ? "user" : "text", text: b.text });
        }
      } else if (b.type === "thinking") {
        var th = b.thinking || b.text || "";
        if (th.trim()) events.push({ id: bid, kind: "thinking", text: th });
      } else if (b.type === "tool_use") {
        var ev = {
          id: bid, kind: "tool", toolId: b.id, name: b.name || "Tool",
          summary: window.__bramTranscriptToolSummary(b.name, b.input),
          result: "", isError: false,
        };
        events.push(ev);
        if (b.id) toolById[b.id] = ev;
      } else if (b.type === "tool_result") {
        var tu = b.tool_use_id;
        if (tu && toolById[tu]) {
          toolById[tu].result = window.__bramTranscriptResultText(b.content).slice(0, 4000);
          toolById[tu].isError = !!b.is_error;
        }
      }
    }
  }
  return events;
};

// Immutable toggle of an id in an array (proven per-item expand pattern,
// matching Workspace's expandedItemIds — avoids object-literal var inits
// that XMLUI's expression engine mishandles).
window.__bramToggleInArray = function (arr, id) {
  arr = arr || [];
  if (arr.indexOf(id) >= 0) return arr.filter(function (x) { return x !== id; });
  return arr.concat([id]);
};
window.startLatestJsonlPolling = function (key, getProvider) {
  key = key || "__bramLatestJsonlPoller";
  if (__latestJsonlPollers[key] && typeof __latestJsonlPollers[key].stop === "function") {
    try { __latestJsonlPollers[key].stop(); } catch (e) {}
  }
  var sinceOffset = 0;
  var sessionSid = "";
  var lastProvider = null;
  var lastTickAt = 0;
  var inFlight = false;
  var stopped = false;
  function providerValue() {
    try {
      return typeof getProvider === "function" ? String(getProvider() || "") : "";
    } catch (e) {
      return "";
    }
  }
  function fetchLatest(force) {
    if (stopped || inFlight) return;
    var now = Date.now();
    if (!force && now - lastTickAt < 2000) return;
    lastTickAt = now;
    var provider = providerValue();
    if (provider !== lastProvider) {
      lastProvider = provider;
      sinceOffset = 0;
      sessionSid = "";
    }
    var url = "/__sessions/latest-tail?provider=" + encodeURIComponent(provider) +
      "&since=" + encodeURIComponent(String(sinceOffset || 0)) +
      "&sid=" + encodeURIComponent(sessionSid || "") +
      "&t=" + encodeURIComponent(String(now));
    inFlight = true;
    window.fetch(url)
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (env) {
        if (!env || stopped) return;
        var content = env.content || "";
        try {
          if (typeof window.logToHost === "function" && !window.__bramMenuPending) {
            window.logToHost({
              kind: "iframe-trace",
              subkind: "jsonl-fanout",
              at: new Date().toISOString(),
              source: "helper",
              len: content.length,
              reset: !!env.reset,
              truncated: !!env.truncated,
            });
          }
        } catch (e) {}
        if (env.reset) {
          window.setLatestJsonl(content);
        } else if (content) {
          window.appendLatestJsonl(content);
        }
        sessionSid = env.sid || "";
        sinceOffset = env.offset || 0;
      })
      .catch(function () {})
      .finally(function () { inFlight = false; });
  }
  var unsubscribe = window.subscribeTalkSessionChange(key + "TalkSessionUnsub", function () {
    fetchLatest(false);
  });
  __latestJsonlPollers[key] = {
    stop: function () {
      stopped = true;
      if (typeof unsubscribe === "function") {
        try { unsubscribe(); } catch (e) {}
      }
      delete __latestJsonlPollers[key];
    },
  };
  fetchLatest(true);
  return __latestJsonlPollers[key].stop;
};
window.startBramLatestJsonlPolling = function (getProvider) {
  if (typeof window.__bramLatestJsonlPollerStop === "function") {
    try { window.__bramLatestJsonlPollerStop(); } catch (e) {}
  }
  window.__bramLatestJsonlPollerStop = window.startLatestJsonlPolling(
    "__bramLatestJsonlPoller",
    getProvider
  );
};
// Convenience: subscribe + remember the unsubscriber on window under the
// caller-supplied key. Avoids `window.X = ...` left-value expressions in
// XMLUI source, which XMLUI's evaluator rejects with "Left value variable
// (X) not found in the scope." The property assignment happens entirely in
// plain JS here; the XMLUI handler just calls this function.
window.subscribeLatestJsonl = function (key, fn) {
  if (typeof window[key] === "function") {
    try { window[key](); } catch (e) {}
  }
  window[key] = window.onLatestJsonlChange(fn);
};
// Append a delta chunk to the shared cache (diff-based latest-tail path,
// issue #100). Caps the cache at __latestJsonlMaxBytes by head-trimming
// at the next newline boundary — keeps the buffer always-valid JSONL so
// downstream parsers (sessionTurns, isWaitingForAssistant, ...) walk
// line-by-line safely. The cap is the missing bound that caused the
// 4138070 revert: without it, every reset:false append grew the buffer
// forever.
var __latestJsonlMaxBytes = 1500000; // ~1.5 MB
window.appendLatestJsonl = function (chunk) {
  if (!chunk) return;
  // Profiling for the responsiveness roadmap (#103-era): pin down which
  // phase costs ~200ms on big appends. Three measurable phases —
  // `concat` is the buffer string-concatenation, `cap` is the cap-check
  // plus optional head-trim, `broadcast` is setLatestJsonl's subscriber
  // dispatch + its own trace log. Sum is `total`.
  var t0 = performance.now();
  var combined = (__latestJsonlValue || "") + chunk;
  var t1 = performance.now();
  var capTrimmed = false;
  if (combined.length > __latestJsonlMaxBytes) {
    capTrimmed = true;
    var beforeLen = combined.length;
    var dropTo = combined.length - __latestJsonlMaxBytes;
    var nl = combined.indexOf("\n", dropTo);
    combined = nl >= 0 ? combined.slice(nl + 1) : combined.slice(dropTo);
    try {
      if (window.logToHost) {
        window.logToHost({
          kind: "iframe-trace",
          subkind: "jsonl-cap-trim",
          at: new Date().toISOString(),
          before: beforeLen,
          after: combined.length,
          dropped: beforeLen - combined.length,
        });
      }
    } catch (e) {}
  }
  var t2 = performance.now();
  window.setLatestJsonl(combined);
  var t3 = performance.now();
  try {
    if (window.logToHost && !window.__bramMenuPending) {
      window.logToHost({
        kind: "iframe-trace",
        subkind: "jsonl-pipeline-ms",
        at: new Date().toISOString(),
        chunkLen: chunk.length,
        bufferLen: combined.length,
        concatMs: Math.round((t1 - t0) * 100) / 100,
        capMs: Math.round((t2 - t1) * 100) / 100,
        capTrimmed: capTrimmed,
        broadcastMs: Math.round((t3 - t2) * 100) / 100,
        totalMs: Math.round((t3 - t0) * 100) / 100,
      });
    }
  } catch (e) {}
};

// Continuous variant: register a callback that fires on every resize
// (window.resize event inside the iframe) plus once with the current
// size at registration time. Use this when you want a readout that
// stays live, not just a snapshot on a button click.
var __rpsSubscriber = null;
var __rpsListenerAttached = false;
function __rpsBroadcast() {
  if (typeof __rpsSubscriber === "function") {
    __rpsSubscriber({
      width: Math.round(window.innerWidth || 0),
      height: Math.round(window.innerHeight || 0),
    });
  }
}
window.subscribeRightPaneSize = function (callback) {
  __rpsSubscriber = typeof callback === "function" ? callback : null;
  if (!__rpsSubscriber) return;
  __rpsBroadcast();
  if (!__rpsListenerAttached) {
    window.addEventListener("resize", __rpsBroadcast);
    __rpsListenerAttached = true;
  }
};
// Push local commits to origin and refetch a DataSource (typically
// the commits list) when the push completes, so the pushed flags
// refresh without a manual reload.
window.gitPush = function (commitsDs, onError) {
  var invoke = getTauriInvoke();
  if (!invoke) return;
  invoke("git_push", {})
    .then(function () {
      if (commitsDs && typeof commitsDs.refetch === "function") {
        commitsDs.refetch();
      }
    })
    .catch(function (e) {
      window.logToHost({ kind: "git-push", phase: "err", error: String(e) });
      if (typeof onError === "function") onError(String(e));
    });
};
// Sessions tab: pending-delete and pending-rename ids persist across
// iframe reloads, so the dim+disable state survives until the user
// explicitly clears it (or the JSONL stops resolving to the same id).
// Two separate keys mirror the in-memory pendingDeletes / pendingRenames
// vars in Sessions.xmlui.
window.loadPendingSessionDeletes = function () {
  try {
    var raw = localStorage.getItem("session-pending-deletes");
    if (!raw) return [];
    var v = JSON.parse(raw);
    return Array.isArray(v) ? v : [];
  } catch (e) { return []; }
};
window.savePendingSessionDeletes = function (ids) {
  try {
    localStorage.setItem("session-pending-deletes", JSON.stringify(ids || []));
  } catch (e) {}
};
window.loadPendingSessionRenames = function () {
  try {
    var raw = localStorage.getItem("session-pending-renames");
    // Clear on read: the dim is meant to signal "agent hasn't picked
    // up the new title yet". A fresh iframe boot (which happens on
    // Bram relaunch, which respawns the PTY child = agent
    // restart) means the dim's job is done. Sessions renamed later in
    // this iframe lifetime stay dimmed via the in-memory append in
    // Sessions.xmlui's onSuccess handler.
    localStorage.removeItem("session-pending-renames");
    if (!raw) return [];
    var v = JSON.parse(raw);
    return Array.isArray(v) ? v : [];
  } catch (e) { return []; }
};
window.savePendingSessionRenames = function (ids) {
  try {
    localStorage.setItem("session-pending-renames", JSON.stringify(ids || []));
  } catch (e) {}
};
// Route external (http/https/file) anchor clicks through openExternal so
// Markdown links and any other <a> tags open in the system browser
// instead of trying to navigate the Tauri WebView (which 404s). Capture
// phase so we run before XMLUI's Markdown-internal click handlers.
//
// Also routes relative *.md anchors (the MEMORY.md cross-references like
// `[foo.md](memory/foo.md)`) to a callback installed via
// registerContextMemorySelector below. We can't intercept these from
// XMLUI's onClick — the event handler cache deep-clones args, so the DOM
// target / preventDefault are gone by the time the XMLUI expression runs.
// And we can't install the window callback from XMLUI either — the
// scripting engine doesn't expose `window`.
var __contextMemorySelector = null;
window.registerContextMemorySelector = function (fn) {
  __contextMemorySelector = typeof fn === "function" ? fn : null;
};
window.clearContextMemorySelector = function () {
  __contextMemorySelector = null;
};
document.addEventListener("click", function (e) {
  var a = e.target && e.target.closest && e.target.closest("a");
  if (!a) return;
  var href = a.getAttribute("href");
  if (!href) return;
  if (/^(https?|file):/i.test(href)) {
    e.preventDefault();
    e.stopPropagation();
    window.openExternal(href);
    return;
  }
  if (href.indexOf("://") === -1 && /\.md(?:[?#].*)?$/i.test(href)) {
    if (typeof __contextMemorySelector === "function") {
      e.preventDefault();
      e.stopPropagation();
      var m = href.match(/([^\/?#]+\.md)(?:[?#]|$)/i);
      var basename = m ? m[1] : "";
      try {
        __contextMemorySelector(basename);
      } catch (err) {
        logToHost({ kind: "memory-link-error", error: String(err && err.message || err) });
      }
    }
  }
}, true);
// Click-driven; scan the DOM per call.
window.scrollAllToTop = function () {
  var root = document.scrollingElement || document.documentElement || document.body;
  if (root) {
    window.scrollTo({ top: 0, behavior: "smooth" });
  }
  var nodes = document.querySelectorAll("*");
  for (var i = 0; i < nodes.length; i += 1) {
    var el = nodes[i];
    if (!el) continue;
    if (el.scrollHeight > el.clientHeight + 8) {
      try {
        el.scrollTo({ top: 0, behavior: "smooth" });
      } catch (e) {
        el.scrollTop = 0;
      }
    }
  }
};
window.scrollAllToBottom = function () {
  var root = document.scrollingElement || document.documentElement || document.body;
  if (root) {
    window.scrollTo({ top: root.scrollHeight, behavior: "smooth" });
  }
  var nodes = document.querySelectorAll("*");
  for (var j = 0; j < nodes.length; j += 1) {
    var sc = nodes[j];
    if (!sc) continue;
    if (sc.scrollHeight > sc.clientHeight + 8) {
      try {
        sc.scrollTo({ top: sc.scrollHeight, behavior: "smooth" });
      } catch (e) {
        sc.scrollTop = sc.scrollHeight;
      }
    }
  }
};
function getTauriInvoke() {
  try {
    if (window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.invoke === "function") {
      return window.__TAURI__.core.invoke.bind(window.__TAURI__.core);
    }
  } catch (e) {}
  try {
    if (window.parent && window.parent.__TAURI__ && window.parent.__TAURI__.core && typeof window.parent.__TAURI__.core.invoke === "function") {
      return window.parent.__TAURI__.core.invoke.bind(window.parent.__TAURI__.core);
    }
  } catch (e) {}
  try {
    if (window.top && window.top.__TAURI__ && window.top.__TAURI__.core && typeof window.top.__TAURI__.core.invoke === "function") {
      return window.top.__TAURI__.core.invoke.bind(window.top.__TAURI__.core);
    }
  } catch (e) {}
  return null;
}
window.addEventListener("message", async (event) => {
  var data = event.data;
  if (!data || data.type !== "inspector-export") return;
  var source = event.source;

  function reply(payload) {
    if (source && typeof source.postMessage === "function") {
      source.postMessage(payload, "*");
    }
  }

  var invoke = getTauriInvoke();
  if (!invoke) {
    reply({ type: "inspector-export-result", ok: false, error: "Tauri IPC unavailable" });
    return;
  }
  try {
    var path = await invoke("save_trace_export", {
      filename: String(data.filename || "xs-trace.json"),
      content: String(data.content || ""),
      mimeType: String(data.mimeType || "application/octet-stream")
    });
    reply({ type: "inspector-export-result", ok: true, path: path });
  } catch (e) {
    logToHost({
      kind: "trace-export-direct-failed",
      error: String((e && e.message) || e),
      at: new Date().toISOString(),
    });
    reply({ type: "inspector-export-result", ok: false, error: String((e && e.message) || e) });
  }
});

// Inspector trace tap (#181). When enabled via the Settings-tab switch
// (traces.inspectorTap in .bram.json), forwards new entries from the
// XMLUI Inspector's window._xsLogs into bram-trace.log as
// [iframe] subkind=inspector-event so they interleave with host traces
// live. Polls at 200 ms with a per-tick cap; overflow emits
// subkind=inspector-overflow. Forwards verbatim — selectivity (filter
// by category, drop per-keystroke noise, etc.) is a follow-up; until
// then the stream carries everything XMLUI logs.
var __inspectorTap = {
  intervalId: null,
  highWater: 0,
  perTickCap: 50,
};
function __inspectorTrace(subkind, fields) {
  try {
    if (typeof window.logToHost !== "function") return;
    var payload = {
      kind: "iframe-trace",
      subkind: subkind,
      at: new Date().toISOString(),
    };
    if (fields && typeof fields === "object") {
      for (var k in fields) {
        if (Object.prototype.hasOwnProperty.call(fields, k)) {
          payload[k] = fields[k];
        }
      }
    }
    window.logToHost(payload);
  } catch (e) {}
}
function __inspectorTapTick() {
  try {
    var logs = window._xsLogs;
    if (!logs || typeof logs.length !== "number") return;
    var total = logs.length;
    if (total <= __inspectorTap.highWater) return;
    var available = total - __inspectorTap.highWater;
    var toSend = Math.min(available, __inspectorTap.perTickCap);
    var t0 = (typeof performance !== "undefined" && performance.now) ? performance.now() : Date.now();
    for (var i = 0; i < toSend; i++) {
      __inspectorTrace("inspector-event", {
        entry: logs[__inspectorTap.highWater + i],
      });
    }
    if (available > toSend) {
      __inspectorTrace("inspector-overflow", {
        dropped: available - toSend,
        totalSeen: total,
      });
      __inspectorTap.highWater = total;
    } else {
      __inspectorTap.highWater += toSend;
    }
    var t1 = (typeof performance !== "undefined" && performance.now) ? performance.now() : Date.now();
    __inspectorTrace("inspector-tap-tick", {
      batch: toSend,
      available: available,
      ms: Math.round((t1 - t0) * 10) / 10,
    });
  } catch (e) {}
}
function __startInspectorTap() {
  if (__inspectorTap.intervalId !== null) return;
  try {
    var logs = window._xsLogs;
    __inspectorTap.highWater =
      logs && typeof logs.length === "number" ? logs.length : 0;
  } catch (e) {
    __inspectorTap.highWater = 0;
  }
  __inspectorTap.intervalId = setInterval(__inspectorTapTick, 200);
}
function __stopInspectorTap() {
  if (__inspectorTap.intervalId === null) return;
  clearInterval(__inspectorTap.intervalId);
  __inspectorTap.intervalId = null;
}
function __applyInspectorTapSetting(enabled) {
  if (enabled) __startInspectorTap();
  else __stopInspectorTap();
}
function __loadInspectorTapSetting() {
  if (typeof window.fetch !== "function") return;
  window
    .fetch("/__settings", { cache: "no-store" })
    .then(function (r) { return r.ok ? r.json() : null; })
    .then(function (s) {
      var enabled = !!(s && s.traces && s.traces.inspectorTap);
      __applyInspectorTapSetting(enabled);
    })
    .catch(function () {});
}
__loadInspectorTapSetting();
try {
  window.subscribeTauriEvent(
    "__bramInspectorTapSettingsUnsub",
    "settings-changed",
    function () { __loadInspectorTapSetting(); }
  );
} catch (e) {}

// Adjustable root font-size for the XMLUI surface (mirrors the terminal-side
// pattern in app/main.js). Buttons in AppHeader call setAppFontSize /
// getAppFontSize. The right pane and the agent tools drawer share origin
// and localStorage; a BroadcastChannel keeps their runtime sizes in lockstep.
(function () {
  var APP_FONT_KEY = "bram.app.fontSize";
  var LEGACY_APP_FONT_KEY = "xmlui-desktop.app.fontSize";
  var APP_FONT_MIN = 10;
  var APP_FONT_MAX = 28;
  var APP_FONT_DEFAULT = 16;

  function clampAppFontSize(n) {
    var v = Math.round(Number(n) || 0);
    if (v < APP_FONT_MIN) v = APP_FONT_MIN;
    if (v > APP_FONT_MAX) v = APP_FONT_MAX;
    return v;
  }

  function applyFontSize(size) {
    try {
      document.documentElement.style.fontSize = size + "px";
    } catch (e) {}
  }

  var bc = null;
  try {
    bc = new BroadcastChannel(APP_FONT_KEY);
    bc.onmessage = function (ev) {
      if (!ev || !ev.data) return;
      applyFontSize(clampAppFontSize(ev.data.size));
    };
  } catch (e) {}

  window.getAppFontSize = function () {
    try {
      var raw = parseInt(
        localStorage.getItem(APP_FONT_KEY) ||
          localStorage.getItem(LEGACY_APP_FONT_KEY) ||
          "",
        10
      );
      return isFinite(raw) ? clampAppFontSize(raw) : APP_FONT_DEFAULT;
    } catch (e) {
      return APP_FONT_DEFAULT;
    }
  };

  window.setAppFontSize = function (n) {
    var size = clampAppFontSize(n);
    applyFontSize(size);
    try {
      localStorage.setItem(APP_FONT_KEY, String(size));
    } catch (e) {}
    if (bc) {
      try { bc.postMessage({ size: size }); } catch (e) {}
    }
    return size;
  };

  window.resetAppFontSize = function () {
    return window.setAppFontSize(APP_FONT_DEFAULT);
  };

  applyFontSize(window.getAppFontSize());
})();

// Surface JS errors and lifecycle events to the host log channel.
window.addEventListener("error", (e) => {
  logToHost({
    kind: "error",
    message: e.message,
    source: e.filename,
    lineno: e.lineno,
    colno: e.colno,
    stack: e.error && e.error.stack,
    at: new Date().toISOString(),
  });
});
window.addEventListener("unhandledrejection", (e) => {
  logToHost({
    kind: "unhandledrejection",
    reason: String(e.reason),
    stack: e.reason && e.reason.stack,
    at: new Date().toISOString(),
  });
});
