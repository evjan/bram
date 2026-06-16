// Slice a file's content into a grep -C style window around a 1-indexed
// target line. Returns [{ line, text, isMatch }, ...]. Used by Context.xmlui
// to render search-hit snippets without re-fetching from the server.
function snippetAroundLine(content, line, context) {
  if (!content || !line) return [];
  const lines = content.split('\n');
  const target = line - 1;
  const ctx = context || 6;
  const start = Math.max(0, target - ctx);
  const end = Math.min(lines.length, target + ctx + 1);
  const slice = [];
  for (let i = start; i < end; i++) {
    slice.push({ line: i + 1, text: lines[i] || '', isMatch: i === target });
  }
  return slice;
}

// Reduce a (potentially huge) turn body to just the paragraphs that
// contain the query (case-insensitive substring). Used by Sessions.xmlui
// after a hit-snippet click so the target app shows context around the
// match instead of the whole turn. Returns the joined paragraphs (still
// valid Markdown for the Markdown component).
function paragraphsContaining(text, query) {
  if (!text) return '';
  const q = (query || '').trim().toLowerCase();
  if (!q) return text;
  const paragraphs = text.split(/\n{2,}/);
  const hits = paragraphs.filter((p) => p.toLowerCase().includes(q));
  return hits.length > 0 ? hits.join('\n\n') : text;
}

function statusSectionSubhead(title) {
  const descriptions = {
    'Startup Run': 'first-minute load',
    'Worklist': 'item lifecycle health',
    'Inflight Sentinel': 'agent action claims',
    'Hooks': 'agent guard setup',
    'Agent Coordination': 'Setup-managed file health',
    'Authorization': 'approval record flow',
    'Latest Tail And Fanout': 'session stream pressure',
    'Guards, Staleness, Interrupts, Traces': 'safety signal trail',
  };
  return descriptions[title] || '';
}

function statusSignalDescription(signal) {
  const descriptions = {
    'Payload maxima': 'Largest startup payloads seen during the first minute. Big tails, fanout bodies, or repeated resets point to sluggish UI, excess JSONL parsing, or trace noise worth trimming.',
    'Renderer drift': 'Measures PTY volume and heartbeat delay during startup. High drift means the UI thread or terminal stream was busy enough to delay visible updates.',
    'Inspector export': 'Shows whether a recent XMLUI Inspector trace exists. A fresh export gives agents concrete interaction, API, and state-change evidence instead of guessing from markup.',
    'Current items': 'Counts active Worklist rows by lifecycle phase. It tells you whether Bram is waiting for apply approval, commit approval, or cleanup.',
    'Recent transitions': 'Summarizes recent Worklist lifecycle snapshots. Use it to confirm approve, apply, commit, and drop actions are moving instead of leaving stale rows behind.',
    'Applied integrity': 'Checks whether applied Worklist items still match the files they changed. A warning means the working tree drifted after apply, so commit approval may no longer describe reality.',
    'Current claim': 'Shows the active host-managed spinner claim. If it is not idle, Bram believes an approve, drop, or iterate cycle is still in progress.',
    'Trace pairs': 'Counts recent inflight sentinel writes and clears. Balanced pairs mean spinner state is being created and cleared through the expected host lifecycle.',
    'Turn completion': 'Reports the latest agent-turn-end decision. It helps explain why a spinner cleared, stayed up, or was skipped when no active claim existed.',
    'Port file': 'Checks Bram port metadata on disk. Stale or mismatched port files explain failed loopback calls and coordination requests that never reach the shell.',
    'Loopback HTTP': 'Probes Bram HTTP on 127.0.0.1. If this fails, agent pane routes, close helpers, or legacy loopback workflows may be unreachable.',
    'Python 3': 'Confirms Python is available for worklist guard hooks. Without it, Claude and Codex hooks may be installed but unable to enforce Bram edits.',
    'Claude hook': 'Shows whether Claude Code has Bram’s PreToolUse guard installed and registered. It protects repo files from unapproved direct edits.',
    'Codex hook': 'Shows whether Codex has Bram’s worklist guard installed and registered. It enforces the same proposal and approval gate for Codex file mutations.',
    'Latest record': 'Shows the newest structured authorization or close-helper record. It helps diagnose stale approvals, consumed payloads, and what the agent is currently allowed to do.',
    'Record age': 'Reports how old the latest coordination record is. Old unconsumed records often explain stuck buttons, stale approvals, or surprising guard behavior.',
    'latest-tail': 'Tracks session-tail polling work. Large or frequent tail reads can make Transcript and Worklist updates feel slow.',
    'JSONL fanout': 'Shows shared JSONL broadcast activity and subscriber count. It helps identify whether session updates are efficiently shared or repeatedly reparsed.',
    'Guard decisions': 'Counts recent guard blocks. Warnings here mean an agent tried to mutate files outside the approved Worklist path.',
    'Stale approvals': 'Counts rejected stale approvals. These happen when Worklist content changed after the user clicked, so the agent must not apply that payload.',
    'Interrupts': 'Shows recent interruption or silence-clear events. These explain why an agent cycle stopped, a spinner cleared, or an active turn ended unexpectedly.',
    'Inspector exports': 'Reports recent Inspector trace availability. Traces give agents exact UI evidence when markup alone does not explain a bug.',
  };
  if (descriptions[signal]) return descriptions[signal];
  const s = signal || '';
  if (s.indexOf('CLAUDE.md') >= 0) return 'Checks Bram guidance embedded for Claude. Missing, stale, or legacy marker blocks can leave Claude following old coordination rules.';
  if (s.indexOf('AGENTS.md') >= 0) return 'Checks Bram guidance embedded for Codex. Missing, stale, or legacy marker blocks can leave Codex following old coordination rules.';
  if (s.indexOf('settings.json') >= 0) return 'Checks Claude hook registration. The hook file can exist but still be ineffective if settings.json does not reference it.';
  if (s.indexOf('config.toml') >= 0) return 'Checks Codex global configuration managed by Bram Setup. Stale hook blocks or developer instructions can strand Codex on old coordination behavior.';
  if (s.indexOf('worklist-guard.py') >= 0) return 'Checks a Bram worklist guard script against the bundled version. Stale scripts can allow unsafe edits or block valid approved changes.';
  if (s.indexOf('bram-conventions.md') >= 0) return 'Checks the shared Bram conventions sidecar. Stale guidance means agents may follow outdated approval, commit, or cleanup rules.';
  return 'Reports one coordination signal from Bram Status. Use its level, state, detail, and timestamp to decide whether setup, mode routing, or agent communication needs attention.';
}

function historyPhaseKind(phase) {
  return window.__bramHistoryPhaseKind(phase);
}
function historyDecodeJsonStringValue(raw) {
  return window.__bramHistoryDecodeJsonStringValue(raw);
}
function historyExtractProseFromDiff(diff) {
  return window.__bramHistoryExtractProseFromDiff(diff);
}
function historyCurrentProsePhase(group) {
  return window.__bramHistoryCurrentProsePhase(group);
}
function historyLatestPhase(group) {
  return window.__bramHistoryLatestPhase(group);
}
function historyCurrentItem(group) {
  return window.__bramHistoryCurrentItem(group);
}
function historyItemProse(item) {
  return window.__bramHistoryItemProse(item);
}
function historyCardProsePreview(group) {
  return window.__bramHistoryCardProsePreview(group);
}
function historyDateParts(iso) {
  return window.__bramHistoryDateParts(iso);
}
function historyDateRangeLine(group) {
  return window.__bramHistoryDateRangeLine(group);
}
function historyPhaseLabel(phase) {
  return window.__bramHistoryPhaseLabel(phase);
}
function historyPhasePath(group) {
  return window.__bramHistoryPhasePath(group);
}
function historyItemFieldMarkdown(group, field) {
  return window.__bramHistoryItemFieldMarkdown(group, field);
}
function historyItemFilesLine(group) {
  return window.__bramHistoryItemFilesLine(group);
}
function historyLatestProseChanged(group) {
  return window.__bramHistoryLatestProseChanged(group);
}
function historyDraftWasMissing(group) {
  return window.__bramHistoryDraftWasMissing(group);
}
function historyItemFate(group) {
  return window.__bramHistoryItemFate(group);
}

// Past transcripts often contain broken docs.xmlui.org/... URLs (the form the
// xmlui-mcp server reports as Source). The live docs are hosted at
// www.xmlui.org/docs/... with a `reference/` segment for component pages.
// Rewrite on the way to Markdown so links resolve when clicked.
function rewriteXmluiDocUrls(text) {
  return window.__bramRewriteXmluiDocUrls(text);
}
function extractImagePaths(text) {
  return window.__bramExtractImagePaths(text);
}
function stripImagePaths(text) {
  return window.__bramStripImagePaths(text);
}
function extractMarkdownImages(text) {
  return window.__bramExtractMarkdownImages(text);
}
function stripMarkdownImages(text) {
  return window.__bramStripMarkdownImages(text);
}

// Iframe-side trace helper for the [iframe] category of the comms-path
// iframeTrace and _traceHelperTiming bodies live in
// app/__shell/helpers.js as plain JS (window.__bramIframeTrace,
// window.__bramTraceHelperTiming). The `__bram` prefix is critical:
// browser top-level `function iframeTrace(...)` declarations hoist
// onto `window.iframeTrace`, so if the window helper were also named
// `iframeTrace`, this xs declaration would overwrite the plain-JS
// implementation, and the delegator's call would recurse into itself.
// The prefixed name keeps the two namespaces independent. Same
// pattern as `applyAgentMenu` / `window.__bramApplyAgentMenu` (commit
// ea9480e).
function iframeTrace(subkind, fields) {
  if (typeof window !== 'undefined' && typeof window.__bramIframeTrace === 'function') {
    window.__bramIframeTrace(subkind, fields);
  }
}

function _traceHelperTiming(name, t0, extra) {
  if (typeof window !== 'undefined' && typeof window.__bramTraceHelperTiming === 'function') {
    window.__bramTraceHelperTiming(name, t0, extra);
  }
}

// Clean a user turn for transcript display: strip protocol prefixes
// (`voice: `, `talk: `) so spoken / typed content reads as plain text;
// summarize structured Worklist lifecycle payloads to a one-line
// action + item label instead of dumping JSON. Anything else passes through.
function formatUserTurnForTranscript(text) {
  return window.__bramFormatUserTurnForTranscript(text);
}
function worklistActionStatusLabel(item) {
  return window.__bramWorklistActionStatusLabel(item);
}
function conversationPaneUserText(text) {
  return window.__bramConversationPaneUserText(text);
}
function worklistActionDisplay(kind, items) {
  return window.__bramWorklistActionDisplay(kind, items);
}
function worklistActionStatusSuffix(item) {
  return window.__bramWorklistActionStatusSuffix(item);
}
function worklistActionConversationDisplay(kind, items, selectedId, feedback) {
  return window.__bramWorklistActionConversationDisplay(kind, items, selectedId, feedback);
}

function toolSummary(name, input) {
  return window.__bramToolSummary(name, input);
}
function parseJsonString(value) {
  return window.__bramParseJsonString(value);
}
function codexToolName(payload) {
  return window.__bramCodexToolName(payload);
}
function codexToolInput(payload) {
  return window.__bramCodexToolInput(payload);
}

function codexToolSummary(payload) {
  return window.__bramCodexToolSummary(payload);
}

// Pretty-print arbitrary tool input as JSON, truncated to N lines.
function toolInputJsonLines(input, maxLines) {
  return window.__bramToolInputJsonLines(input, maxLines);
}

// Concatenate the text content of a tool_result block (handles both
// string and array-of-blocks shapes).
function toolResultText(content) {
  return window.__bramToolResultText(content);
}

// True if a tool_result block carries an error (either flagged via
// is_error or detected by an Error:/<tool_use_error> prefix). Used to
// tint the inline result banner red.
function isErrorResult(block) {
  return window.__bramIsErrorResult(block);
}

function codexToolOutput(payload) {
  return window.__bramCodexToolOutput(payload);
}

// Shallow turn equality: enough to tell "unchanged turn" from
// "changed/new" without doing a full JSON.stringify. Used by sessionTurns
// to preserve object refs for stable turns so XMLUI's Items doesn't
// re-mount the whole list on every poll.
function turnsLooselyEqual(a, b) {
  return window.__bramTurnsLooselyEqual(a, b);
}

// Parse a slice of JSONL lines into the turn-list shape sessionTurns
// returns. `toolIndex` (optional) lets the caller pre-populate the
// tool_use_id → entry map so cross-boundary tool_results in an
// incremental parse can still find their originating tool. Returns
// only the turns generated from `lines` (no structural-share — that's
// the caller's responsibility). Extracted from sessionTurns so the
// full-parse and incremental paths share it (issue #100).
function _parseLinesToTurns(lines, toolIndex) {
  return window.__bramParseLinesToTurns(lines, toolIndex);
}

function sessionTurns(jsonlText) {
  return window.__bramSessionTurns(jsonlText);
}

// Worklist close-issue dialog state helpers. The dialog opens when a TO COMMIT
// item carries closesIssues: [N, ...]. State shape is { <issueNumber>: { close,
// comment } } so per-issue checkbox + comment edits update one branch without
// disturbing the rest. Immutable updates so XMLUI's reactivity refreshes.
function initCloseIssueState(closesIssues) {
  const state = {};
  for (const entry of (closesIssues || [])) {
    const n = (entry && typeof entry === 'object') ? entry.number : entry;
    state[n] = { close: true, comment: '' };
  }
  return state;
}
function normalizeCloseIssue(entry) {
  if (entry && typeof entry === 'object') {
    return {
      number: entry.number,
      title: (entry.title || '').trim(),
    };
  }
  return {
    number: entry,
    title: '',
  };
}
function setCloseIssueClose(state, n, close) {
  const prev = (state && state[n]) || { close: true, comment: '' };
  return Object.assign({}, state || {}, { [n]: Object.assign({}, prev, { close: !!close }) });
}
function setCloseIssueComment(state, n, comment) {
  const prev = (state && state[n]) || { close: true, comment: '' };
  return Object.assign({}, state || {}, { [n]: Object.assign({}, prev, { comment: comment || '' }) });
}
// Produce the `close-issue:` lines the agent reads out of the approved
// payload's feedback. Lines look like `close-issue: 52` or
// `close-issue: 52 comment: "shipped"`. JSON.stringify on the comment keeps
// embedded quotes / newlines unambiguous for the agent's parse.
function buildCloseIssueLines(state) {
  const lines = [];
  for (const key of Object.keys(state || {})) {
    const v = state[key];
    if (!v || !v.close) continue;
    const c = (v.comment || '').trim();
    if (c) lines.push('close-issue: ' + key + ' comment: ' + JSON.stringify(c));
    else lines.push('close-issue: ' + key);
  }
  return lines;
}
// Merge user-typed feedback with the dialog-generated close-issue lines.
// Empty base + no lines → empty string; otherwise lines come after the user's
// text separated by a blank line so the agent can split on `\n\n`.
function combineFeedbackWithCloseLines(base, lines, pushBeforeClose) {
  const baseTrim = (base || '').trim();
  const generated = [];
  if (pushBeforeClose) generated.push('push-before-close: true');
  if (lines && lines.length > 0) generated.push.apply(generated, lines);
  if (generated.length === 0) return baseTrim;
  if (!baseTrim) return generated.join('\n');
  return baseTrim + '\n\n' + generated.join('\n');
}

function closeIssuePushScopeRows(item, commits) {
  const rows = [];
  if (item && item.id) {
    rows.push({
      sha: '(new)',
      subject: item.id,
      relation: 'Approved worklist item',
    });
  }
  for (const c of (commits || [])) {
    if (!c || c.pushed) continue;
    rows.push({
      sha: (c.sha || '').slice(0, 7),
      subject: (c.commit && c.commit.message) || '',
      relation: 'Already pending on this branch',
    });
  }
  return rows;
}

function closeIssueExistingPendingCount(commits) {
  return (commits || []).filter(function (c) { return c && !c.pushed; }).length;
}

// Worklist-hotspot instrumentation helpers (`Workspace.xmlui` per-item
// Approve / Iterate / Drop + closeIssues dialog). Each helper calls
// `App.mark(label)` — the xmlui-native, sandbox-safe replacement for
// the soon-to-be-banned `performance.*` family (see plan #17 step 2.5
// in the xmlui repo). `App` is spread into xs-script expression scope
// the same way `formatDate` / `navigate` / etc. are, so these helpers
// can live alongside the other Globals.xs functions — no separate
// window-global script needed. App.mark pushes a `kind: "app:mark"`
// record with `ts` (Unix ms) and `perfTs` to the inspector buffer,
// directly mergeable with bram-trace.log on the same Unix-ms clock.
function traceIterateEnabled(submitting, selected, selectedFeedback) {
  return window.__bramTraceIterateEnabled(submitting, selected, selectedFeedback);
}
function traceApproveDropEnabled(submitting, selected) {
  return window.__bramTraceApproveDropEnabled(submitting, selected);
}
function buildApprovePayload(items, selectedId, feedback) {
  return window.__bramBuildApprovePayload(items, selectedId, feedback);
}
function buildIteratePayload(items, selectedId, feedback) {
  return window.__bramBuildIteratePayload(items, selectedId, feedback);
}
function buildDropPayload(items, selectedId, feedback) {
  return window.__bramBuildDropPayload(items, selectedId, feedback);
}
function buildSingleItemApprovePayload(itemRef, feedback) {
  return window.__bramBuildSingleItemApprovePayload(itemRef, feedback);
}
function countByStatus(items, status) {
  return window.__bramCountByStatus(items, status);
}
function buildBatchApprovePayload(items, feedback) {
  return window.__bramBuildBatchApprovePayload(items, feedback);
}
function buildBatchDropPayload(items, feedback) {
  return window.__bramBuildBatchDropPayload(items, feedback);
}

function settingsAgent(s) {
  return (s && s.shell && s.shell.agent) || '';
}
function settingsBatch(s) {
  return !!(s && s.worklist && s.worklist.batchCommitActions);
}
function settingsMinimized(s) {
  return !!(s && s.ui && s.ui.targetAppMinimized);
}
function settingsInspectorTap(s) {
  return !!(s && s.traces && s.traces.inspectorTap);
}
function settingsTracingEnabled(s) {
  return !!(s && s.traces && s.traces.enabled);
}
// Default OFF — only explicit `true` enables. Matches the host
// default; the setting is opt-in. Bram-on-Bram developers turn
// this on if they want source-edit hot-reload; everyone else
// experiences no observable difference either way (their edits
// trigger right-pane-reload, not tools-pane-reload).
function settingsToolsPaneHotReload(s) {
  if (!s || !s.ui) return false;
  return s.ui.toolsPaneHotReload === true;
}

// Diff rendering — used by the DiffView component, which all three
// diff sites (Transcript, Workspace, Commits) share. Per-line
// classification + theme-variable backgrounds; no syntax highlighter
// is bundled with xmlui-standalone so we hand-classify.
function diffLineRows(text) {
  if (!text) return [];
  return text.split('\n').map(function (line) {
    let kind = 'context';
    if (line.startsWith('@@')) kind = 'hunk';
    else if (line.startsWith('+++') || line.startsWith('---')) kind = 'fileheader';
    else if (line.startsWith('diff ') || line.startsWith('index ')) kind = 'fileheader';
    else if (line.startsWith('+')) kind = 'add';
    else if (line.startsWith('-')) kind = 'del';
    return { kind: kind, text: line || ' ' };
  });
}
function diffLineBg(kind) {
  if (kind === 'add') return '$color-success-100';
  if (kind === 'del') return '$color-danger-100';
  if (kind === 'hunk') return '$color-info-100';
  return 'transparent';
}
function diffLineColor(kind) {
  if (kind === 'fileheader') return '$textColor-secondary';
  return '$textColor-primary';
}

// Normalize either the backend's annotated rows (with optional per-line
// `segments`) or, as a fallback while the backend round-trip is in
// flight, the locally-classified rows from diffLineRows(). Returns rows
// in a single uniform shape DiffView can iterate: each row carries
// row-level `bg`/`color` plus a non-empty `segments` array. Segments
// without their own `bg` render transparent (no intra-line emphasis).
function diffViewRows(apiResult, fallbackText) {
  const raw = (apiResult && apiResult.length) ? apiResult : diffLineRows(fallbackText);
  return raw.map(function (row) {
    const lineColor = diffLineColor(row.kind);
    const segs = (row.segments && row.segments.length)
      ? row.segments
      : [{ text: row.text }];
    return {
      kind: row.kind,
      bg: diffLineBg(row.kind),
      color: lineColor,
      segments: segs.map(function (s) {
        return { text: s.text, bg: s.bg || null, color: lineColor };
      }),
    };
  });
}

// Build a unified-diff string from an Edit/MultiEdit tool's
// old_string/new_string so DiffView can render it the same way it
// renders git's output.
function unifiedDiffFromEdit(input) {
  if (!input) return '';
  const oldLines = (input.old_string || '').split('\n');
  const newLines = (input.new_string || '').split('\n');
  const head = '--- a\n+++ b\n';
  const hunk = '@@ -1,' + oldLines.length + ' +1,' + newLines.length + ' @@\n';
  const minus = oldLines.map(function (l) { return '-' + l; }).join('\n');
  const plus  = newLines.map(function (l) { return '+' + l; }).join('\n');
  const body = (oldLines.length && newLines.length) ? (minus + '\n' + plus) : (minus + plus);
  return head + hunk + body;
}

// Feedback route helpers — parallel to the history* family. The Feedback
// component browses entries from /__feedback-history/list, each shaped as
// { ts: <unix_ms>, itemId: <string>, fileName: <string> }.
function feedbackHistoryItemTitle(entry) {
  return (entry && entry.itemId) || '(unknown item)';
}
function feedbackHistoryDateLine(entry) {
  if (!entry || !entry.ts) return '';
  const d = new Date(Number(entry.ts));
  if (isNaN(d.getTime())) return '';
  return d.toISOString().slice(0, 16).replace('T', ' ');
}

// Centered context window around the first case-insensitive match of
// `query` in `body`, capped at `maxChars`. Returns three string segments
// (before, match, after) plus truncation flags so the renderer can emit
// "…" affordances. Used by both the search-result card snippet preview
// (~160 chars) and the shared SearchHitModal (~500 chars).
function searchHitWindow(body, query, maxChars) {
  const cap = maxChars || 500;
  const src = body || '';
  const q = query || '';
  if (!src || !q) {
    return {
      before: src.slice(0, cap),
      match: '',
      after: '',
      truncatedLeft: false,
      truncatedRight: src.length > cap,
    };
  }
  const idx = src.toLowerCase().indexOf(q.toLowerCase());
  if (idx < 0) {
    return {
      before: src.slice(0, cap),
      match: '',
      after: '',
      truncatedLeft: false,
      truncatedRight: src.length > cap,
    };
  }
  const halfRemainder = Math.max(0, Math.floor((cap - q.length) / 2));
  let start = Math.max(0, idx - halfRemainder);
  let end = Math.min(src.length, start + cap);
  if (end - start < cap) start = Math.max(0, end - cap);
  return {
    before: src.slice(start, idx),
    match: src.slice(idx, idx + q.length),
    after: src.slice(idx + q.length, end),
    truncatedLeft: start > 0,
    truncatedRight: end < src.length,
  };
}

// Pick the first candidate body that actually contains the query
// (case-insensitive). Falls back to the first non-empty candidate if no
// candidate matches. Used by hit-row click handlers where the "primary"
// body (e.g., an issue's main body) may not contain the match — the
// match might live in a comment, an author name, etc. — but the
// per-hit `snippet` always does. Try richer bodies first for better
// context, fall back to snippet last.
function searchHitBestBody(query, candidates) {
  if (!candidates || !candidates.length) return '';
  if (!query) {
    for (const c of candidates) {
      if (c) return c;
    }
    return '';
  }
  const q = query.toLowerCase();
  for (const c of candidates) {
    if (c && c.toLowerCase().indexOf(q) >= 0) return c;
  }
  for (const c of candidates) {
    if (c) return c;
  }
  return '';
}

// ---- Worklist "message agent" box: persistence + lifecycle helpers ----
// Kept here so Workspace.xmlui can stay markup-only per xmlui_rules #9.

// Worklist message-agent persistence + lifecycle. Bodies live in
// app/__shell/helpers.js as plain JS (window.__bram*). Same migration
// shape and naming convention as the iframeTrace / agent-menu work:
// distinct `__bram`-prefixed window names dodge the trap where xs
// `function foo` would hoist onto `window.foo` and overwrite the
// helpers.js implementation
// (see memory: xs-to-window-migration-name-collision).
function restoreWorklistDraft() {
  return window.__bramRestoreWorklistDraft();
}
function persistWorklistDraft(text) {
  window.__bramPersistWorklistDraft(text);
}
function clearWorklistDraft() {
  window.__bramClearWorklistDraft();
}
function restoreConversationOpen() {
  return window.__bramRestoreConversationOpen();
}
function toggleConversationOpen(current) {
  return window.__bramToggleConversationOpen(current);
}
function restoreWorklistConversationLayout() {
  return window.__bramRestoreWorklistConversationLayout();
}
function setWorklistConversationLayout(layout) {
  return window.__bramSetWorklistConversationLayout(layout);
}
function restoreWorklistUiState(field) {
  return window.__bramRestoreWorklistUiState(field);
}
function persistWorklistUiState(state) {
  window.__bramPersistWorklistUiState(state);
}
function clearWorklistUiState() {
  window.__bramClearWorklistUiState();
}
function restoreWorklistAwaiting() {
  return window.__bramRestoreWorklistAwaiting();
}
function restoreWorklistAwaitingSetAt() {
  return window.__bramRestoreWorklistAwaitingSetAt();
}
function markAwaitingStarted() {
  return window.__bramMarkAwaitingStarted();
}
function restoreWorklistSubmittedMessage() {
  return window.__bramRestoreWorklistSubmittedMessage();
}
function restoreWorklistSubmittedKind() {
  return window.__bramRestoreWorklistSubmittedKind();
}
function setWorklistSubmittedKind(kind) {
  return window.__bramSetWorklistSubmittedKind(kind);
}
function restoreWorklistSubmittedBaseline() {
  return window.__bramRestoreWorklistSubmittedBaseline();
}
function submitWorklistMessageFast(text) {
  return window.__bramSubmitWorklistMessageFast(text, worklistVoiceTarget);
}
function withStagedImageMarkers(text, target) {
  return window.__bramWithStagedImageMarkers(text, target, worklistVoiceTarget);
}
function recordWorklistFeedbackConversation(text) {
  return window.__bramRecordWorklistFeedbackConversation(text);
}
function clearWorklistAwaiting(clearDraft) {
  window.__bramClearWorklistAwaiting(clearDraft);
}

// Mirrors toTurn's `s.replace(/\s+/g, ' ').trim()` collapse in
// app/__shell/helpers.js so the JSONL-recorded user text (post-collapse)
// can be matched against the locally-stored submittedWorklistMessage
// (pre-collapse). Strict === would fail whenever the submitted text
// contained any internal whitespace runs.
// Map the inflight sentinel's `kind` field to the gerund verb shown below
// the in-flight item ("Approving", "Iterating", "Dropping"). Returns '' for
// unknown / missing kind so the calling markup hides cleanly.
function inflightActionLabel(kind) {
  return window.__bramInflightActionLabel(kind);
}
function stripImageMarkerPrefix(text) {
  return window.__bramStripImageMarkerPrefix(text);
}
function worklistSubmittedMatches(exchangeUserText, submitted) {
  return window.__bramWorklistSubmittedMatches(exchangeUserText, submitted);
}
function markTurnEnded(via, state) {
  return window.__bramMarkTurnEnded(via, state);
}
function computeConversationSync(state) {
  return window.__bramComputeConversationSync(state);
}

// Per-tab splitter persistence. XMLUI's documented `resize` event
// delivers the primary panel size in pixels, while older traces showed
// `[primary, secondary]` arrays. Preserve both forms: pixel events are
// stored as `Npx`, arrays are normalized to a percentage.
// Note: `writeLocalStorage('bram.splitter.<key>', v)` does
// persist to native localStorage, but XMLUI nests dotted keys under
// the top-level — value lands at `localStorage.bram.splitter.<key>`
// inside the JSON blob at `localStorage['bram']`, not as a flat
// `localStorage['bram.splitter.<key>']` entry. A flat-key sqlite
// probe will miss it; check the `bram` top-level instead.
// Keys: `bram.splitter.<key>` (worklist, sessions, commits, context,
// issues). The outer-shell `bram.splitter.shell` key is owned by
// app/main.js and uses native localStorage flat keys.
function restoreSplitterSize(key, fallback) {
  return window.__bramRestoreSplitterSize(key, fallback);
}
function saveSplitterSize(key, sizes) {
  window.__bramSaveSplitterSize(key, sizes);
}

var worklistVoiceTarget = '';
var worklistVoiceText = '';
var worklistVoiceMeta = null;
var worklistVoiceSeq = 0;
var worklistVoiceProcessing = false;
var worklistVoiceProcessingTarget = '';
// True between mediaRecorder actually starting in the parent shell and the
// user clicking stop / a transcript arriving. Drives the tri-state voice
// buttons so they show ⏳ during the start-up gap (parent runs
// ensureServerRunning + getUserMedia + new MediaRecorder before
// mediaRecorder.start() fires) instead of ⏹ instantly. Without this the
// iframe button flips to ⏹ synchronously and users start speaking into a
// not-yet-recording stream, losing the first phoneme(s).
var worklistVoiceRecordingActive = false;
var bramWorklistVoiceTarget = '';

function isWorklistTextVoiceTarget(target) {
  return window.__bramIsWorklistTextVoiceTarget(target);
}

function setWorklistVoiceTarget(target) {
  const next = target || '';
  bramWorklistVoiceTarget = next;
  if (worklistVoiceTarget === next) return;
  worklistVoiceTarget = next;
  iframeTrace('voice-input', { target: worklistVoiceTarget || 'terminal', stage: 'target' });
}

function isWorklistVoiceProcessingTarget(target) {
  return !!worklistVoiceProcessing && worklistVoiceProcessingTarget === (target || '');
}

window.bramCurrentPasteTarget = function () {
  return bramWorklistVoiceTarget || worklistVoiceTarget || '';
};

// Decide the iframe-side state update for the `inflightClaim` DataSource
// (the wrapper around resources/.inflight-claim.json). Sentinel is the
// single source of truth for the spinner. Returns an object the caller
// destructures and assigns; xs scope rules prevent us from writing
// App-level vars from a function defined here (that's the same lvalue
// constraint we hit on the active-tool path in 525a718). Kinds:
//   - 'submit' : sentinel claims an item; caller sets submitting +
//                submittedItemId + actionProgressKind.
//   - 'clear'  : sentinel went empty after a submitting state; caller
//                runs the cleanup block (and emits the iframe-clear trace
//                with the returned trace payload).
//   - 'none'   : no transition needed.
//
// IMPORTANT non-resets in the 'clear' branch:
//
//   - stickyConversationTools / stickyConversationToolsKey
//     INTENTIONALLY not reset here. Mirror of b54e9b1 in onSubmit:
//     the sticky cache is the cross-turn bridge that keeps the
//     `Agent: Tool uses` section mounted between the end of one
//     turn and the first raw tool of the next. Resetting causes a
//     transient unmount when lastExchangeDS hasn't refetched yet --
//     diagnosed as the "transient flash of tool uses" symptom.
//     The ChangeListener that updates sticky will overwrite it as
//     soon as the next turn produces its own raw tools.
//
//   - setWorklistVoiceTarget('message-agent') IS called in the
//     'clear' branch (and also via the reactive listener below).
//     Belt-and-suspenders: after an action cycle completes, the
//     feedback panel unmounts along with the ChangeListener that
//     delivers worklistVoiceText into feedbackBox. If
//     worklistVoiceTarget stayed 'feedback', the next voice cycle's
//     transcript would land nowhere (only the message-agent
//     ChangeListener is always mounted, and it gates on the target
//     matching). The reactive listener below covers every other
//     path that unmounts the feedback panel.
function inflightSentinelDecide(data, prevSubmitting, prevSubmittedItemId) {
  return window.__bramInflightSentinelDecide(data, prevSubmitting, prevSubmittedItemId);
}

// Reset worklistVoiceTarget to 'message-agent' whenever the feedback
// panel is no longer mounted. Mounted condition:
//   selected !== null AND feedbackExpanded === true.
//
// When the panel unmounts via any path (radio-dot click on a different
// row, inflight-clear, item swap from the worklist, etc.), the feedback
// ChangeListener that consumes worklistVoiceText goes with it. The
// message-agent ChangeListener is always mounted but gates on
// target === 'message-agent', so a stale 'feedback' target drops
// transcripts on the floor (diagnosed 2026-06-10 06:18:12: [voice]
// stage=voice-into-result fired, no subkind=voice-input stage=append).
// Wired into a ChangeListener at the Workspace VStack so every
// transition that affects panel mount-state triggers the check.
function resetVoiceTargetIfFeedbackPanelGone(selected, feedbackExpanded) {
  if (!selected || !feedbackExpanded) {
    setWorklistVoiceTarget('message-agent');
  }
}

function appendVoiceTranscript(component, transcript) {
  if (!component || !transcript) return false;
  const meta = worklistVoiceMeta || {};
  const current = String(component.value || '');
  const cleaned = transcript.replace(/\r?\n/g, ' ').replace(/[ \t]+/g, ' ').trim();
  if (!cleaned) return false;
  const spacer = current && !/\s$/.test(current) ? ' ' : '';
  const appended = spacer + cleaned;
  const next = current + appended;
  component.setValue(next);
  const restore = () => {
    let focused = false;
    let cursorAtEnd = false;
    if (typeof component.focus === 'function') {
      component.focus();
      focused = true;
    }
    if (typeof component.setSelectionRange === 'function') {
      component.setSelectionRange(next.length, next.length);
      cursorAtEnd = true;
    }
    iframeTrace('voice-input', {
      target: worklistVoiceTarget || 'message-agent',
      stage: 'append',
      requestId: meta.requestId || null,
      stopAtMs: meta.stopAtMs || null,
      stopToAppendMs: typeof meta.stopAtMs === 'number' ? Date.now() - meta.stopAtMs : null,
      stopToResultMs: typeof meta.stopToResultMs === 'number' ? meta.stopToResultMs : null,
      parentStopToDeliverMs:
        typeof meta.parentStopToDeliverMs === 'number' ? meta.parentStopToDeliverMs : null,
      chars: cleaned.length,
      rawChars: transcript.length,
      focused,
      cursorAtEnd
    });
  };
  delay(0);
  restore();
  return true;
}

// Toolbar PTY keystroke instrumentation for #182 incident 6: tracks
// the iframe's current view of pendingMenu at the moment the user
// clicks a toolbar button (1/2/3/Yes/No/Esc), so post-hoc analysis
// can tell whether the click landed on a menu that was actually
// still open vs one the host had already cleared.
// setToolbarPendingMenuFromEvent / setToolbarPendingMenuFromTurnState /
// traceToolbarKey live in app/__shell/helpers.js as window globals.
// xs callers (Main.xmlui's subscribeTauriEvent callbacks and the
// toolbar onClick handlers) resolve them via bare-name window lookup
// — same pattern as logToHost / toTurn / sendKeys. No xs declarations
// here so there's no statement-queue cost or hoist-collision risk.

// Menu state moved into helpers.js (window.bramAgentMenu et al). The
// xs setters below are thin delegators kept for any caller still
// hitting them from xs scope; the actual work, including
// `listener-fired` trace emission, lives in window.__bramApply* /
// window.__bramSetAgentMenu* and runs in plain JS to skip XMLUI's
// processStatementQueueAsync per-statement awaits
// (xmlui/src/components-core/script-runner/process-statement-async.ts:115-166).
// Source of truth: window.bramAgentMenu. Read it directly from xs
// (this file) and from xmlui markup (Main.xmlui suppression gates,
// AgentMenu.xmlui via getAgentMenu).

function applyAgentMenu(menu, suppressFallback, source) {
  if (typeof window !== 'undefined' && typeof window.__bramApplyAgentMenu === 'function') {
    return window.__bramApplyAgentMenu(menu, suppressFallback, source);
  }
  return false;
}

function setAgentMenuFromTurnState(turnState, surface) {
  if (typeof window !== 'undefined' && typeof window.__bramSetAgentMenuFromTurnState === 'function') {
    window.__bramSetAgentMenuFromTurnState(turnState, surface);
  }
}

function setAgentMenuFromEvent(e, surface) {
  if (typeof window !== 'undefined' && typeof window.__bramSetAgentMenuFromEvent === 'function') {
    window.__bramSetAgentMenuFromEvent(e, surface);
  }
}

function getAgentMenu(turnState) {
  const current = (typeof window !== 'undefined') ? window.bramAgentMenu : null;
  const suppress = (typeof window !== 'undefined') ? window.bramAgentMenuSuppressFallback : true;
  return current || (!suppress && turnState && turnState.pendingMenu) || null;
}

// Toolbar PTY delegators. Required even though the actual work lives
// on window.__bram*: XMLUI's expression engine analyzes identifiers
// inside arrow-function bodies passed to subscribeTauriEvent (e.g.,
// Main.xmlui's onInit), and a bare name with no xs declaration causes
// silent registration failure that aborts the rest of the onInit and
// cascades into AgentMenu's mount. With these declarations present
// xs callers — Main.xmlui's subscriber arrows and the toolbar button
// onClick handlers — resolve as expected.
function setToolbarPendingMenuFromEvent(e) {
  if (typeof window !== 'undefined' && typeof window.__bramSetToolbarPendingMenuFromEvent === 'function') {
    window.__bramSetToolbarPendingMenuFromEvent(e);
  }
}
function setToolbarPendingMenuFromTurnState(turnState) {
  if (typeof window !== 'undefined' && typeof window.__bramSetToolbarPendingMenuFromTurnState === 'function') {
    window.__bramSetToolbarPendingMenuFromTurnState(turnState);
  }
}
function traceToolbarKey(key) {
  if (typeof window !== 'undefined' && typeof window.__bramTraceToolbarKey === 'function') {
    window.__bramTraceToolbarKey(key);
  }
}

function toggleVoiceForCurrentTarget(recording) {
  if (recording) {
    const stoppingTarget = worklistVoiceTarget || '';
    worklistVoiceRecordingActive = false;
    worklistVoiceProcessing = true;
    worklistVoiceProcessingTarget = stoppingTarget;
    iframeTrace('voice-input', { target: stoppingTarget || 'terminal', stage: 'processing-start' });
    voiceStop((t, meta) => {
      worklistVoiceProcessing = false;
      worklistVoiceProcessingTarget = '';
      if (!t) {
        iframeTrace('voice-input', { target: stoppingTarget || 'terminal', stage: 'processing-empty' });
        return;
      }
      if (isWorklistTextVoiceTarget(worklistVoiceTarget)) {
        worklistVoiceText = t;
        worklistVoiceMeta = meta || null;
        worklistVoiceSeq = worklistVoiceSeq + 1;
        iframeTrace('voice-input', {
          target: worklistVoiceTarget || 'message-agent',
          stage: 'stop',
          requestId: meta && meta.requestId ? meta.requestId : null,
          stopAtMs: meta && meta.stopAtMs ? meta.stopAtMs : null,
          stopToResultMs: meta && typeof meta.stopToResultMs === 'number' ? meta.stopToResultMs : null
        });
      } else {
        iframeTrace('voice-input', { target: worklistVoiceTarget || 'terminal', stage: 'fallback-terminal' });
        toTurn('voice: ' + t);
      }
      iframeTrace('voice-input', { target: stoppingTarget || 'terminal', stage: 'processing-end' });
    });
    return false;
  }
  iframeTrace('voice-input', { target: worklistVoiceTarget || 'terminal', stage: 'start' });
  worklistVoiceRecordingActive = false;
  worklistVoiceProcessing = false;
  worklistVoiceProcessingTarget = '';
  voiceStart(() => {
    worklistVoiceRecordingActive = true;
    iframeTrace('voice-input', { target: worklistVoiceTarget || 'terminal', stage: 'recording-started' });
  });
  return true;
}

// isWorklistActionPayloadText lives in app/__shell/helpers.js as a
// window helper — the xs-script parser choked on regex-literal versions
// here. Bare-name calls resolve via the window scope (same as toTurn,
// logToHost, queueFeedbackDraft).
