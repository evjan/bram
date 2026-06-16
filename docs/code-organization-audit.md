# Code organization audit

This audit maps where iframe-side code actually lives across four surfaces,
measures it against the project's stated discipline, and lists concrete drift
to file as follow-up worklist items. Snapshot date: 2026-06-16. Sources read:
`app/__shell/helpers.js` (3563 lines), `app/tools/Globals.xs` (964 lines),
`app/tools/Main.xmlui`, and the 18 components under `app/tools/components/`.

## The discipline

This audit revisits the rules. The version captured in `CLAUDE.md` /
`conventions.md` / the user-memory entries assumed that every helpers.js
helper needed a matching xs delegator. The refined version below drops
that assumption — it's what the current state of the codebase actually
calls for.

### The rules

- **Pure functions** (sync, no engine-hostile primitives, no XMLUI
  component state) → live on `window` in `app/__shell/helpers.js`.
  XMLUI markup calls them as `window.foo(...)`.
- **Shims for outside-sandbox operations** (async, `fetch`, `setTimeout`,
  `postMessage`, tauri event listeners) → also live on `window` in
  `helpers.js`, because the XMLUI expression engine can't host them.
  XMLUI markup calls them as `window.foo(...)`.
- **xs-only code** — `Globals.xs` holds: xs-scope module state
  (declarations like `var worklistVoiceTarget = ''` whose readers and
  writers all live in xs), and pure helpers whose proximity to that
  state earns them a place there. Everything else should be in
  `helpers.js`.
- **XMLUI attribute handlers** — a single function call:
  `onClick="window.foo(...)"` or `onClick="window.__bramFoo(...)"`. Never
  multi-statement bodies, never multi-line arrow bodies, never
  object-literal blobs. (No change from the previous version of the
  rules.)
- **The `__bram*` namespace prefix** — used to defend
  `helpers.js`-side names against collision with `function foo`
  declarations in `Globals.xs` (which auto-hoist onto `window.foo`).
  Bare-name helpers on `window` are safe as long as `Globals.xs` does
  NOT declare a function of the same name. If we stop adding xs
  delegators (see next section), the prefix's defensive purpose
  shrinks — it stays useful only for the names that *do* still have an
  xs-side counterpart.

### When and why do we need delegators?

**Almost never.** A delegator —
`function foo(...) { return window.__bramFoo(...); }` in `Globals.xs` —
exists to let XMLUI markup write the bare name `foo(...)` instead of
`window.__bramFoo(...)`. That's the only thing it buys.

There is *one* specific corner where the bare name actually matters:
inside arrow-function bodies passed to handlers
(`subscribeTauriEvent`/`onDidChange`/`onLoaded`), the XMLUI expression
engine's identifier analyzer can silently abort registration if it sees
a bare `foo` that isn't a known identifier. The xs `function foo`
declaration declares the identifier and unblocks the analyzer. (Captured
in `feedback_xmlui_arrow_body_needs_xs_decl.md`.)

But the same arrow body can call `window.__bramFoo(...)` directly with no
analyzer trouble at all. The delegator is only required if you insist on
writing the bare name.

**The cost.** Every `function foo` in `Globals.xs` auto-hoists onto
`window.foo`. That hoisting *is* the collision mechanism the
`__bram*` namespace exists to defend against. Each delegator we add
opens the very collision window the discipline was built to close.

**The rule.** Default to no delegator. Call helpers as `window.foo(...)`
from XMLUI markup. Add a delegator only when (a) the function is called
many times in attribute expressions where the seven-character `window.`
prefix is genuinely annoying, and (b) the name doesn't already clash with
anything on the bare `window` surface.

The 86 delegators currently in `Globals.xs` are mostly fossil from the
prior model. Most of them could be deleted with no code change beyond
prefixing their call sites with `window.`. The inventory and drift
findings below are reframed accordingly.

## Surfaces

### `app/__shell/helpers.js` — 224 `window.*` assignments

Grouped by purpose:

| Group | Count | Notes |
|---|---|---|
| `window.__bram*` helpers | ~85 | 1:1 with the 86 xs delegators in `Globals.xs`. Under the refined discipline, the `__bram*` prefix only earns its keep on names where a `Globals.xs` `function foo` actually still exists; once the delegators are pared down, most of these can drop the prefix or stay as-is at our discretion. No duplicate name-definitions found. |
| Bare `bram*` (no underscore) state vars + helpers | ~20 | Paste-image / voice / agent-menu state. Under the *old* discipline these were drift (collision risk vs. xs hoisting). Under the *refined* discipline they're fine, because no `Globals.xs` declaration currently shadows them. They become drift only if someone adds a matching xs delegator. See *Drift* §1. |
| Bare-name XMLUI public surface | ~30 | Documented shims: `toShell`, `toTurn`, `sendKeys`, `logToHost`, `openExternal`, `captureScreenshot`, `gitPush`, `voiceStart`, `voiceStop`, `getRightPaneSize`, `subscribeRightPaneSize`, `subscribeTalkSessionChange`, `subscribeTauriEvent`, `appendLatestJsonl`, `setLatestJsonl`, `onLatestJsonlChange`, `startBramLatestJsonlPolling`, `subscribeLatestJsonl`, `scrollAllToTop`, `scrollAllToBottom`, `queueFeedbackDraft`, `sendIterateWithFeedbackDraft`, `getAppFontSize`/`setAppFontSize`/`resetAppFontSize`, `recordToolbarPendingMenuFromEvent`, `getToolbarPendingMenuState`, `registerContextMemorySelector`/`clearContextMemorySelector`. |
| Underscore-internal `_foo` | 6 | `_xsLogs`, `_fetchLogged`, `_voiceSession`, `_voiceStartedListener`, `_scrollables`, `_talkPinned`. |
| Cross-window write | 1 | `parent.__bramTauriListenerUnsubs` — deliberate; keys live on the parent so unsubs survive iframe reloads. |
| Likely-dead surface | 7 | `loadChecked`, `saveChecked`, `pruneChecked`, `markInflight`, `getInflight`, `clearInflight`, `wasPinnedToBottom`/`scrollAfterDomUpdate` family. See *Dead code* below. |

**No duplicate definitions** of the same name from different code paths
inside helpers.js. State vars are written from multiple sites but always
from within their own helper.

### `app/tools/Globals.xs` — 86 functions + 10 top-level vars

| Classification | Count | Refined-discipline verdict |
|---|---|---|
| **thin-delegator** — body is one `return window.__bramFoo(...)` (with optional `typeof window` guard) | 86 | Drift. Each one hoists `function foo` onto `window.foo` purely for syntactic convenience. Default action: delete the delegator, update XMLUI call sites to `window.__bramFoo(...)`. Keep only the few where bare-name calls in arrow bodies provide real value. |
| **real-logic-engine-ok** — pure sync data manipulation that xs handles cleanly | 30 | Mixed. Candidates whose only XMLUI caller is a binding-string need-bare-name use case can stay; pure data helpers with no xs-scope dependency move to helpers.js. |
| **real-logic-engine-risky** — multi-callback / arrow bodies / xs-scope side-effects that arguably belong in helpers.js | 3 | Drift. Move to `helpers.js`. |
| **dead-delegator** — calls a `window.__bramFoo` that doesn't exist | 0 | — |
| **non-function** — top-level `var` declarations (xs-scope module state) | 10 | Correct location. xs-scope state genuinely belongs in xs. |

The three risky outliers all live on the voice path:

- `handleFeedbackVoiceArrival` (line 786) — 17-line xs function: calls
  sibling `appendVoiceTranscript`, reads xs-scope `worklistVoiceText`,
  `Object.assign` builds a new map, multi-stage `iframeTrace` emissions,
  persists via delegator. Exactly the shape that belongs in helpers.
- `appendVoiceTranscript` (line 804) — defines an arrow body
  `restore = () => { ... }` inside xs and calls it after `delay(0)`.
  Arrow bodies in xs are the pattern flagged by
  `feedback_xmlui_arrow_body_needs_xs_decl.md`.
- `toggleVoiceForCurrentTarget` (line 917) — two nested arrow callbacks
  (`voiceStop(...)`, `voiceStart(...)`) that branch and mutate five
  xs-scope vars.

The 30 "real-logic-engine-ok" entries (settings readers, diff classifiers,
close-issue dialog state machine, search-hit windowing, status-row tables,
feedback-history formatters, unified-diff assembly) are intentional and
reasonable — pure sync data work, no async-shaped code, no engine-hostile
primitives. Migrating them would be busywork without payoff.

### `window.*` — the union

The deduped picture:

- **86 `__bram*` names** — one per Globals.xs delegator → helpers.js
  helper. Clean namespace, no collisions.
- **~20 `bram*` names without the underscore prefix** — split into two
  visible clusters: paste-image state (`bramPendingPastedImageCount`,
  `bramPendingPastedImagePaths`, `bramStagingPastedImageCount`,
  `bramConsumePastedImagePaths`, `bramRemovePastedImagePath`,
  `bramTracePastedImageStrip`, plus the per-target variants) and
  voice/focus mirrors (`bramAgentMenu`, `bramAgentMenuSuppressFallback`,
  `bramAgentMenuLastHostMs`, `bramAgentMenuLastSource`,
  `bramActiveVoiceTargetMirror`, `bramSetActiveVoiceTargetMirror`,
  `bramActiveFocusedFeedbackItemIdMirror`,
  `bramSetActiveFocusedFeedbackItemIdMirror`,
  `bramCurrentPasteTarget`, `bramPastedImageForCurrentTurn`,
  `bramPastedImageTarget`, `bramLastConsumedPastedImages`,
  `bramPasteImageTraceSigs`, `bramStagePastedImage`,
  `bramLastConsumedPastedImagePaths`, `bramHasPendingPastedImages`,
  `bramPendingPastedImageCountForTarget`,
  `bramPendingPastedImagePathsForTarget`).
- **~30 bare-name names** — the documented host-helper shims plus
  some that probably should be documented (e.g. `scrollAllToTop`,
  `scrollAllToBottom`, `sendKeys`).
- **6 `_foo` names** — internal, owned by helpers.js.
- **One xs-side conflict** — `bramWorklistVoiceTarget` declared in
  `Globals.xs:690` mirrors `worklistVoiceTarget` (line 676) at every
  write site, but has zero readers anywhere in the project (confirmed
  by grep). See *Dead code* below.

**Same-name collisions across both writers: none observed.** The
`__bram*` names are written only from helpers.js. The bare `bram*` names
are written from helpers.js only. xs delegators read the corresponding
`__bram*` helper but never reassign a name that helpers.js owns.

### XMLUI attribute handlers — the fourth de facto surface

Threshold: more than one statement; or multi-line arrow body; or string
literal spanning >120 chars; or operators beyond a single call.

**Roughly 100 attribute bodies violate the threshold across all 19
`.xmlui` files.** Severity distribution:

| Severity | Approx. count | Shape |
|---|---|---|
| **trivial** | ~40 | Two-statement state resets, short ternary side effects, single-conditional arrow blocks. Could be inlined as one-liners with minor edits. |
| **moderate** | ~35 | One helper extraction would clean each one up. Most are repeated patterns (search-hit-open, displayedX ternary, dialog-close reset). |
| **major** | ~25 | Multi-statement, mixes concerns, must move to `window.__bramX` + thin xs delegator. The Approve/Iterate/Drop family in `Workspace.xmlui` is the bulk. |

Workspace.xmlui is the offender catalog. Five handlers exceed 1200 chars
each; the largest single body, `VStack.onInit` at line 300, is ~3000
chars with six nested `subscribeTauriEvent` blocks. The Approve handler
(L746, ~2100 chars), Iterate (L761, ~1900), Drop (L776, ~1700),
Confirm-close-issues (L1001/L1005, ~1800/~1700), and the two
form-submit cascades (L488/L517, ~1200/~1700) are all near-clones of a
single state-mutation cascade.

**Cross-cutting patterns repeated across files** (single helper would
collapse N call sites):

| Pattern | Sites |
|---|---|
| `imageSrcForPath(path)` — nested ternary with `data:` check + `file://` regex | 7 (AgentLastResponse L29/L50, ConversationPane L8/L29, PastedImageStrip L17, Sessions L65/L260, Workspace L467) |
| Search-hit-open — `searchHitBody = …; searchHitTitle = …; modalRef.open()` | 6 (Commits L138, Feedback L111, History L159, Issues L284, Sessions L204, Status L82) |
| Pasted-image state refresh — three-line `pendingX = window.bram…ForTarget ? … : 0` cascade | 9 (Workspace L213/L487/L517/L746/L761/L776/L1001/L1005, PastedImageStrip L11) |
| `displayedX` ternary — `(query && query.trim().length >= 2) ? searchResults.results : fullList` | 4 (Feedback L20, History L20, Issues L79, Sessions L48) |
| 4–5-statement dialog-close reset | 6 (Issues L101/L141/L194, Workspace L1107/L1189/L1194) |
| Sort-toggle for table headers (`if (sortField === 'x') sortDir = …; else sortField = 'x'`) | 3 (Issues L209/L216/L224) |
| Header-label nested ternary on `mainAgentStatus`/`enhanceStatus.value` | 5 (Main L159/L164/L176/L181/L192) |

## Drift findings

### 1. The 86 thin delegators in `Globals.xs` are themselves the drift

Under the refined discipline (above), most of the delegators currently
in `Globals.xs` are syntactic sugar with a real cost: each one hoists
`function foo` onto `window.foo`, expanding the surface where future
collisions can happen. The remediation is mechanical: delete the
delegator, replace bare-name call sites in XMLUI markup with the
qualified `window.__bramFoo(...)`. Arrow-body call sites can also use
`window.__bramFoo(...)` directly — the only reason to keep a delegator is
if a measurable share of binding-string call sites genuinely benefit
from the shorter form. Most don't.

The 86 names span 18 functional groupings (history-formatters,
worklist-action payload builders, codex tool parsers, persistence
helpers, agent-menu setters, etc.). They can be retired in groups, not
all at once.

### 2. The `bram*` (no underscore) cluster — formerly drift, now not

Roughly 20 names sit on `window.bramFoo` without the `__bram*` prefix.
Under the prior discipline this was flagged as forward-looking collision
risk against potential future xs `function bramFoo` declarations. Under
the refined discipline, the answer is to NOT add such declarations — the
prefix becomes a tool for the specific cases that still need it, not a
blanket rule.

These names are correctly placed today. They become drift only if
someone adds a matching xs delegator. (If we follow item 1 and pare
down delegators, that risk recedes further.)

The implication for `feedback_xs_to_window_migration_name_collision.md`:
the failure that memory records was real, but the fix wasn't "prefix
everything `__bram`" — it was "don't declare an xs function with the
same name as a helpers.js export." The refined discipline encodes that
directly.

### 2. Two raw `fetch().then().catch()` calls in XMLUI attribute handlers

`Main.xmlui:117` and `Main.xmlui:284` use raw `window.fetch` inside
`onClick`, in violation of `CLAUDE.md`'s "no async / fetch outside
DataSource" rule. Both wrap their `then` in attribute strings and re-fetch
a DataSource on success.

This is the same anti-pattern that
`feedback_xmlui_no_complex_expressions_in_attributes.md` was written to
flag, plus a bonus violation: raw `fetch` in an XMLUI attribute is
explicitly listed in `CLAUDE.md` as something that gets rejected at
evaluation time. Both currently work because `then`/`catch` are inside the
expression engine's tolerance, but they're a documented foot-gun.

### 3. Three voice-path xs functions carry real logic the engine handles awkwardly

`handleFeedbackVoiceArrival`, `appendVoiceTranscript`,
`toggleVoiceForCurrentTarget` in `Globals.xs` (lines 786, 804, 917). All
three use arrow bodies inside xs, all three mutate multiple xs-scope state
vars, all three emit multi-stage iframeTrace calls. Per
`feedback_xmlui_arrow_body_needs_xs_decl.md`, arrow bodies inside xs are
where the engine's identifier analysis silently aborts registration. These
are the unfinished tail of the helpers.js migration.

The xs-scope vars they mutate (`worklistVoiceTarget`, `worklistVoiceText`,
`worklistVoiceMeta`, `worklistVoiceSeq`, `worklistVoiceProcessing`,
`worklistVoiceProcessingTarget`, `worklistVoiceRecordingActive`) already
have window-mirror counterparts (`bramActiveVoiceTargetMirror`, etc.) or
could trivially gain them.

### 4. Worklist action handlers are an 8x duplicated cascade

`Workspace.xmlui` lines 488, 517, 746, 761, 776, 1001, 1005 each implement
a near-identical "submit + reset state" cascade of 15–25 statements:

- Extract feedback (raw or stored)
- Wrap with image markers
- Build display text + record conversation
- If `sent`, set 7 state vars
- Reset pasted-image state (3 vars)
- Set 5 more state vars
- Emit `inflight-set` trace
- Call `toTurn(...)` with the right payload builder

They differ only by payload kind and a handful of mode-specific values.
One `window.__bramSubmitWorklistAction(mode, $item, state, setters)`
helper would replace all 7 (plus the half-migrated form-submit at L488 and
the skip-worklist at L517 which already call
`submitWorklistMessageFast`).

This is also where the half-migrated pattern is most visible: L488 and
L517 already call `window.__bramFlushWorklistDraft` and
`submitWorklistMessageFast` — the helpers exist; the rest of the handler
just kept going inline.

### 5. The largest single XMLUI body, `Workspace.xmlui:300`, is six event subscribers

A single `VStack.onInit` attribute of ~3000 chars containing six
`subscribeTauriEvent` calls, each with a multi-statement arrow body and
nested `debounce`. The same shape (single onInit, multiple subscribes)
appears at `Main.xmlui:2` (~1800 chars), `AgentMenu.xmlui:5`, and
`Sessions.xmlui:115`. None of these have been extracted.

### 6. Image-src binding is copy-pasted 7 times

The `(path).startsWith('data:') || ... ? path : '/__file?path=' +
encodeURIComponent(path.replace(/^file:\/\/(localhost)?/, ''))` ternary
ships verbatim in 7 places. Single `imageSrcForPath(path)` xs function
collapses all of them.

### 7. Search-hit-open is the same 3-statement pattern in 6 places

Listed in *Surfaces*. One `window.__bramOpenSearchHit(body, title,
modalRef)` helper + one xs delegator.

### 8. Header-label ternaries on `mainAgentStatus`/`enhanceStatus.value` are five identical fall-throughs in `Main.xmlui`

Lines 159, 164, 176, 181, 192 all walk the same `mainAgentStatus` →
`enhanceStatus.value` fall-through to pick a label. One
`formatHeaderProviderLabel`/`formatHeaderFinishedLabel` xs function family
unifies them.

### 9. The `listenTo` binding strings are doing structural work in markup

Examples: `Main.xmlui:13` is a 7-way string concatenation pulling 5
fields out of `mainAgentStatus`; `AgentLastResponse.xmlui:4` joins
`assistantText` + image URLs as a signature; `Workspace.xmlui:110` is a
5-flag status string; `Workspace.xmlui:250` is a 13-way concatenation of
`conversationStateDS.value.lastExchange` fields. Each of these is a
"signature for a ChangeListener" computation that belongs in xs as a
small pure function.

### 10. `Settings.xmlui` carries ~870 chars of help-text body inline

Lines 40, 63, 79, 99 are info-dialog openers whose `infoBody` strings are
plain prose embedded in attributes. The L99 body is ~870 chars with
embedded HTML entities — it's the bulk of the file. One
`window.__bramOpenSettingsInfo(key)` with the bodies stored as a constants
object in helpers.js removes the inline literals entirely.

## Dead code candidates

### Confirmed dead (no readers anywhere)

| Symbol | Location | Confirmation |
|---|---|---|
| `bramWorklistVoiceTarget` xs var | `Globals.xs:690` | Write at L698 inside `setWorklistVoiceTarget`. No readers in `app/__shell/helpers.js`, `app/tools/`, or anywhere else (grep -r). It mirrors `worklistVoiceTarget` (line 676) which IS read; the mirror has no consumer. |
| `window.loadChecked` / `saveChecked` / `pruneChecked` | `helpers.js:3097/3105/3150` | Only referenced inside `helpers.js` itself (`pruneChecked` calls the other two). No XMLUI or xs caller. The "workspace-checked" persistence appears to have been superseded. |
| `window.markInflight` / `getInflight` / `clearInflight` | `helpers.js:3066/3080/3090` | Only referenced inside helpers.js + a stale comment in `app/tools/index.html:34-36` that names them as examples in markup. No live XMLUI/xs caller. The host-managed inflight sentinel (`resources/.inflight-claim.json` per `conventions.md`) supersedes this. |
| `window.wasPinnedToBottom` / `window.scrollAfterDomUpdate` | `helpers.js:3493/3502` | `scrollAfterDomUpdate` calls `wasPinnedToBottom`; nothing else does. No XMLUI/xs reader. |
| `window.isWorklistActionPayloadText` | `helpers.js:436` | Only referenced in a comment at `Globals.xs:961` ("lives in helpers.js as a…"). No active reader. |

### Probable dead — needs investigation before removal

| Symbol | Location | Note |
|---|---|---|
| `window.fetch` wrap | `helpers.js:14-25` | Self-described as "Temporary instrumentation for the queryParams investigation". The investigation referenced isn't in the open worklist; if it has resolved, the wrap should come out (it intercepts every fetch in the iframe). |

### Probable not-dead but undocumented

| Symbol | Location | Note |
|---|---|---|
| `window.sendKeys`, `window.captureScreenshot` | `helpers.js:377/2010` | Both are part of the public API surface used in XMLUI markup (`sendKeys` is called 7 times across `Main.xmlui` and `AgentMenu.xmlui`; `captureScreenshot` is referenced in `Architecture.xmlui` documentation tables) but neither is in `CLAUDE.md`'s helper table. Document or namespace. |

## Recommended follow-up worklist items

Reordered against the refined discipline. New helpers default to
**no xs delegator** — XMLUI markup calls `window.foo(...)` directly. Add
a delegator only when explicit need is shown.

### Codify the refined discipline first

1. **`codify-refined-organization-discipline`** — update
   `app/__shell/conventions.md` (and the relevant memory entries) so the
   refined model is the canonical one: pure functions and shims live on
   `window` in `helpers.js`; XMLUI markup calls `window.foo(...)`; xs
   delegators are only justified when measurably worth the namespace
   cost; the `__bram*` prefix is for names that have an xs counterpart,
   not a blanket rule. Files: `app/__shell/conventions.md`, possibly
   `CLAUDE.md`. Without this, items 2–4 keep getting re-litigated.

### Cleanup that doesn't touch the discipline

2. **`audit-dead-code-removal`** — delete the seven confirmed-dead
   symbols (`bramWorklistVoiceTarget`, `loadChecked`/`saveChecked`/
   `pruneChecked`, `markInflight`/`getInflight`/`clearInflight`,
   `wasPinnedToBottom`/`scrollAfterDomUpdate`,
   `isWorklistActionPayloadText`) plus the stale `index.html` comment
   that names the inflight family. Verify each with grep before deletion.
3. **`investigate-window-fetch-wrap`** — find or close the "queryParams
   investigation" referenced in `helpers.js:17`. If resolved, delete the
   wrap; if not, file the open question explicitly.
4. **`document-or-namespace-sendkeys-captureScreenshot`** — `sendKeys` is
   called 7 times in markup; `captureScreenshot` is in the architecture
   doc. Add both to the helper table in `CLAUDE.md` (or rename if we'd
   rather they be private).

### Migrate engine-risky code to helpers.js

5. **`migrate-voice-path-to-helpers`** — move
   `handleFeedbackVoiceArrival` / `appendVoiceTranscript` /
   `toggleVoiceForCurrentTarget` from `Globals.xs` to
   `window.__bram*` (or unprefixed if no name conflict) in `helpers.js`.
   Migrate the seven xs-scope voice vars to window mirrors. Drop the xs
   functions; update call sites to `window.foo(...)`.
6. **`replace-raw-fetch-in-xmlui`** — convert `Main.xmlui:117` and
   `Main.xmlui:284` raw `fetch().then().catch()` blocks to either an
   `<APICall>` component or a `window.__bramX` helper. Closes the
   "no fetch outside DataSource" violation.

### Collapse copy-pasted XMLUI markup

7. **`extract-image-src-helper`** — single `window.imageSrcForPath(path)`
   in `helpers.js`; replace 7 markup call sites across
   `AgentLastResponse`, `ConversationPane`, `PastedImageStrip`,
   `Sessions`, `Workspace`.
8. **`extract-search-hit-open-helper`** — `window.openSearchHit(body,
   title, modalRef)` in `helpers.js`; replace 6 call sites across
   `Commits`, `Feedback`, `History`, `Issues`, `Sessions`, `Status`.
9. **`consolidate-worklist-action-cascade`** — single
   `window.submitWorklistAction(mode, $item, state, setters)` (modes:
   `approve` / `iterate` / `drop` / `approve-close` / `approve-no-close` /
   `message` / `skip-worklist`); replace the 7-handler family in
   `Workspace.xmlui`. Biggest single XMLUI-bytes win.
10. **`extract-onInit-event-subscribers`** — extract `Workspace.xmlui:300`
    (~3000 chars), `Main.xmlui:2` (~1800 chars), `AgentMenu.xmlui:5`,
    `Sessions.xmlui:115` to one `window.foo` per component.
11. **`extract-settings-info-bodies`** — `window.openSettingsInfo(key)`
    reading from a constants table in `helpers.js`; replace
    `Settings.xmlui` L40/L63/L79/L99 inline strings (~870-char L99 body
    is the bulk of the file).
12. **`extract-listenTo-signature-helpers`** — convert the
    binding-string signature concatenations (`Main.xmlui:13`,
    `AgentLastResponse.xmlui:4`, `Workspace.xmlui:110`,
    `Workspace.xmlui:250`) to pure functions on `window`.
13. **`extract-displayedX-helper`** — single
    `window.selectDisplayed(query, searchResults, fullList)`; replace 4
    call sites.
14. **`extract-dialog-close-reset-helpers`** — per-dialog 4–5-statement
    resets in `Issues.xmlui` (3 sites) and `Workspace.xmlui` (3 sites).
15. **`extract-issue-sort-toggle`** — single helper for the 3 copies in
    `Issues.xmlui:209/216/224`.
16. **`extract-header-label-helpers`** —
    `window.formatHeaderProviderLabel`, `window.formatHeaderFinishedLabel`
    for the 5 nested ternaries in `Main.xmlui`.

### Pare delegators (after the discipline is codified)

17. **`pare-globals-xs-delegators`** — group-by-group, delete delegators
    from `Globals.xs` and update XMLUI call sites to
    `window.__bramFoo(...)`. Filable per group (history, payload
    builders, codex tool parsers, persistence, image extraction, etc.).
    Each group is its own approvable item — there's no need to do them
    all at once. Net effect: `Globals.xs` shrinks dramatically;
    `helpers.js` is unchanged; XMLUI markup is slightly noisier at call
    sites but the namespace footprint contracts.

### Suggested order

Items 1 (discipline), 2 (dead code), 5 (voice migration), 6 (raw fetch),
and 9 (worklist-action cascade) cover the largest sources of pain this
audit surfaced. Items 7, 8, 10–16 are independent cleanup that any of us
can pick up in any order. Item 17 is a large refactor whose value lands
only after item 1 — don't start it until the discipline is canon.
