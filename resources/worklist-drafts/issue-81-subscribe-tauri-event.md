# Before

`Workspace.xmlui`'s `onInit` (lines 120-178) registers three Tauri
event listeners directly with the racy unlisten dance:

```js
const tauri = (window.parent && window.parent.__TAURI__) || window.__TAURI__;
if (tauri && tauri.event && tauri.event.listen) {
  if (typeof window.__bramWorklistUnlisten === 'function') { window.__bramWorklistUnlisten(); window.__bramWorklistUnlisten = null; }
  tauri.event.listen('worklist-changed', handler)
    .then((unlisten) => { window.__bramWorklistUnlisten = unlisten; }).catch(() => {});
  // ... same shape for inflight-claim-changed and pty-menu-changed
}
```

`tauri.event.listen` returns a Promise; the unlisten fn is stored on
`window` only after it resolves. xmlui hot-reload re-runs `onInit` on
the persistent iframe window, and each re-run calls
`tauri.event.listen` again on `window.parent.__TAURI__` (which outlives
the component). The revoke is skipped whenever the prior mount's
`.then` hasn't resolved (slot still `null`), so listeners accumulate —
the issue's live trace shows the `pty-menu-changed` handler firing
1× → 2× → 3× as a session accrues Workspace edits (88 of 94 menu emits
fired more than once).

The two helper-backed subscriptions in the same `onInit`
(`subscribeLatestJsonl`, `subscribeTalkSessionChange`, lines 118-119)
do **not** stack: `subscribeTalkSessionChange` registers its parent
listener **once at helpers.js load** and fans out to a synchronous
subscriber array, so `onInit` re-runs only touch a synchronous
keyed-slot path, never `tauri.event.listen`.

# After

Generalize that proven pattern. Add `subscribeTauriEvent(key,
eventName, fn)` to `app/__shell/helpers.js` (next to
`subscribeTalkSessionChange`):

- One parent listener **per eventName**, registered lazily on first
  subscribe and guarded by a module-level flag so it attaches exactly
  once per helpers.js load. It fans out to a per-eventName subscriber
  array, passing the event object through so payload-consuming handlers
  still get `e`.
- `subscribeTauriEvent(key, eventName, fn)` is fully synchronous:
  revoke any prior `window[key]` (splice from the array), push `fn`,
  store a synchronous unsub closure on `window[key]`. No Promise
  window, so `onInit` re-runs keep the live-subscriber count at exactly
  one.

Then replace the three racy blocks in `Workspace.xmlui`'s `onInit`
(removing the `(() => { const tauri = ...; if (...) { ... } })()` IIFE
and all three unlisten dances) with three flat calls:

```js
subscribeTauriEvent('__bramWorklistUnsub', 'worklist-changed', (e) => { ... worklist.refetch(); });
subscribeTauriEvent('__bramInflightClaimUnsub', 'inflight-claim-changed', () => { inflightTick = inflightTick + 1; });
subscribeTauriEvent('__bramPtyMenuUnsub', 'pty-menu-changed', (e) => { pendingMenu = e && e.payload ? e.payload : null; });
```

Handler bodies are unchanged (same `iframeTrace` calls, same refetch /
state writes).

**Operational note:** both files live under `app/`, so saving them
triggers `tools-pane-reload` and the drawer iframe hot-reloads with the
new code — the fixed `onInit` stops accumulating new listeners
immediately. But listeners already leaked onto the persistent
`window.parent.__TAURI__` during this session only clear when that
webview is recreated, i.e. on a **Bram restart**. So a clean
single-fire baseline (and end-to-end verification via `bram-trace.log`)
needs a restart, even though no `cargo build` is required.

Alternatives considered:

- Synchronous mount-generation counter checked inside each handler
  (the issue's *original* proposed shape) — **rejected:** tolerates the
  leaked listeners and short-circuits them at the work boundary; the
  helper removes the leak at its source and matches the already-proven
  `subscribeTalkSessionChange` design.
- Register the parent listener truly once across iframe reloads
  (state on `window.top`) — **rejected:** more moving parts than the
  proven per-helpers.js-load registration, which is sufficient because
  the frequent re-entry path (`onInit`) no longer calls
  `tauri.event.listen` at all.
- **[chosen]** Generalize `subscribeTalkSessionChange` into
  `subscribeTauriEvent` and route all three listeners through it.
