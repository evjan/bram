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

function currentSourceFile(pathname) {
  if (pathname === '/sessions') return 'components/Sessions.xmlui';
  if (pathname === '/') return 'components/Transcript.xmlui';
  if (pathname === '/worklist') return 'components/Workspace.xmlui';
  if (pathname === '/status') return 'components/Status.xmlui';
  return 'Main.xmlui';
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
  if (s.indexOf('CLAUDE.md') >= 0) return 'Checks Bram guidance embedded for Claude. Missing, stale, or legacy marker blocks can leave Claude following old worklist rules.';
  if (s.indexOf('AGENTS.md') >= 0) return 'Checks Bram guidance embedded for Codex. Missing, stale, or legacy marker blocks can leave Codex following old worklist rules.';
  if (s.indexOf('settings.json') >= 0) return 'Checks Claude hook registration. The hook file can exist but still be ineffective if settings.json does not reference it.';
  if (s.indexOf('config.toml') >= 0) return 'Checks Codex global configuration managed by Bram Setup. Stale hook blocks or developer instructions can strand Codex on old coordination behavior.';
  if (s.indexOf('worklist-guard.py') >= 0) return 'Checks a Bram worklist guard script against the bundled version. Stale scripts can allow unsafe edits or block valid approved changes.';
  if (s.indexOf('bram-conventions.md') >= 0) return 'Checks the shared Bram conventions sidecar. Stale guidance means agents may follow outdated approval, commit, or cleanup rules.';
  return 'Reports one coordination signal from Bram Status. Use its level, state, detail, and timestamp to decide whether setup, worklist flow, or agent communication needs attention.';
}

function historyPhaseKind(phase) {
  const summary = ((phase && phase.summary) || '').toLowerCase();
  if (summary.indexOf('applied') >= 0) return 'applied';
  if (summary.indexOf('proposed') >= 0) return 'proposed';
  return '';
}

function historyDecodeJsonStringValue(raw) {
  if (!raw) return '';
  try {
    return JSON.parse('"' + raw + '"');
  } catch (err) {
    return raw.replace(/\\n/g, '\n').replace(/\\"/g, '"');
  }
}

function historyExtractProseFromDiff(diff) {
  const lines = (diff || '').split('\n');
  let before = '';
  let after = '';
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const afterMatch = line.match(/^\+\s+"after":\s+"(.*)"[,]?$/);
    if (afterMatch) {
      after = historyDecodeJsonStringValue(afterMatch[1].replace(/",?$/, ''));
      continue;
    }
    const beforeMatch = line.match(/^\+\s+"before":\s+"(.*)"[,]?$/);
    if (beforeMatch) {
      before = historyDecodeJsonStringValue(beforeMatch[1].replace(/",?$/, ''));
    }
  }
  return after || before;
}

function historyCurrentProsePhase(group) {
  const item = historyCurrentItem(group);
  const itemProse = historyItemProse(item);
  if (itemProse) {
    return {
      phase: historyLatestPhase(group),
      prose: itemProse,
      source: 'snapshot'
    };
  }
  const phases = (group && group.phases) || [];
  for (let i = phases.length - 1; i >= 0; i--) {
    const prose = historyExtractProseFromDiff(phases[i].diff || '');
    if (prose) {
      return { phase: phases[i], prose, source: 'diff' };
    }
  }
  return { phase: null, prose: '', source: '' };
}

function historyLatestPhase(group) {
  const phases = (group && group.phases) || [];
  return phases.length > 0 ? phases[phases.length - 1] : null;
}

function historyCurrentItem(group) {
  return (group && group.currentItem) || null;
}

function historyItemProse(item) {
  if (!item) return '';
  const after = typeof item.after === 'string' ? item.after.trim() : '';
  if (after) return after;
  const before = typeof item.before === 'string' ? item.before.trim() : '';
  return before;
}

function historyCurrentProsePreview(group) {
  const current = historyCurrentProsePhase(group).prose || '';
  const lines = current.split('\n');
  const preview = lines.slice(0, 8).join('\n');
  if (preview.length <= 700) {
    return preview;
  }
  return preview.slice(0, 700).trimEnd() + '\n...';
}

function historyCardProsePreview(group) {
  const current = historyCurrentProsePhase(group).prose || '';
  const normalized = current.replace(/\s+/g, ' ').trim();
  if (!normalized) return '';
  if (normalized.length <= 240) return normalized;
  return normalized.slice(0, 237).trimEnd() + '...';
}

function historyDateParts(iso) {
  if (!iso) return { date: '', time: '' };
  const d = new Date(iso);
  if (isNaN(d.getTime())) {
    return { date: iso.slice(0, 10), time: iso.slice(11, 16) };
  }
  const pad = (n) => String(n).padStart(2, '0');
  return {
    date: d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate()),
    time: pad(d.getHours()) + ':' + pad(d.getMinutes())
  };
}

function historyDateRangeLine(group) {
  const phases = (group && group.phases) || [];
  if (!phases.length) return '';
  const first = historyDateParts((phases[0] || {}).iso || '');
  const last = historyDateParts((phases[phases.length - 1] || {}).iso || '');
  if (first.date && first.date === last.date) {
    return 'On ' + first.date + ' from ' + first.time + ' to ' + last.time;
  }
  return 'From ' + first.date + ' ' + first.time + ' to ' + last.date + ' ' + last.time;
}

function historyPhaseLabel(phase) {
  const summary = ((phase && phase.summary) || '').toLowerCase();
  if (summary.indexOf('committed') >= 0) return 'Committed';
  if (summary.indexOf('applied') >= 0) return 'Applied';
  if (summary.indexOf('proposed') >= 0) return 'Proposed';
  if (summary.indexOf('dropped') >= 0 || summary.indexOf('pruned') >= 0) return 'Dropped';
  return (phase && phase.summary) || 'Changed';
}

function historyPhasePath(group) {
  const phases = (group && group.phases) || [];
  const labels = [];
  for (let i = 0; i < phases.length; i++) {
    const label = historyPhaseLabel(phases[i]);
    if (labels[labels.length - 1] !== label) labels.push(label);
  }
  return labels.join(' -> ');
}

function historyItemFieldMarkdown(group, field) {
  const item = historyCurrentItem(group);
  const value = item && typeof item[field] === 'string' ? item[field].trim() : '';
  return value || '';
}

function historyItemFilesLine(group) {
  const item = historyCurrentItem(group);
  if (!item) return '';
  if (Array.isArray(item.files)) return item.files.join(', ');
  if (typeof item.file === 'string') return item.file;
  return '';
}

function historyProseProvenance(group) {
  const current = historyCurrentProsePhase(group);
  const kind = historyPhaseKind(current.phase) || historyPhaseKind({ summary: group && group.prosePhaseSummary });
  const fate = historyItemFate(group);
  if (historyDraftWasMissing(group)) {
    return 'Draft missing:';
  }
  if (kind === 'applied') {
    const changed = historyLatestProseChanged(group);
    if (fate === 'Fate: committed.') {
      return changed ? 'As committed, changed from proposal:' : 'As committed, unchanged from proposal:';
    }
    return changed ? 'As applied, changed from proposal:' : 'As applied, unchanged from proposal:';
  }
  if (kind === 'proposed') {
    return fate === 'Fate: committed.' ? 'As committed:' : 'As proposed:';
  }
  return 'No prose:';
}

function historyLatestProseChanged(group) {
  const phase = historyLatestPhase(group);
  const diff = (phase && phase.diff) || '';
  return diff.indexOf('"before"') >= 0 || diff.indexOf('"after"') >= 0;
}

function historyDraftWasMissing(group) {
  const item = historyCurrentItem(group);
  return !!(item && item._draftMissing);
}

function historyItemFate(group) {
  const phases = (group && group.phases) || [];
  for (let i = phases.length - 1; i >= 0; i--) {
    const summary = ((phases[i] && phases[i].summary) || '').toLowerCase();
    if (summary.indexOf('committed') >= 0) return 'Fate: committed.';
    if (summary.indexOf('dropped') >= 0 || summary.indexOf('pruned') >= 0) return 'Fate: dropped.';
  }
  return 'Fate: still active.';
}

function historyItemJson(group) {
  return JSON.stringify(group || {}, null, 2);
}

// Past transcripts often contain broken docs.xmlui.org/... URLs (the form the
// xmlui-mcp server reports as Source). The live docs are hosted at
// www.xmlui.org/docs/... with a `reference/` segment for component pages.
// Rewrite on the way to Markdown so links resolve when clicked.
function rewriteXmluiDocUrls(text) {
  if (!text) return text;
  return text
    .replace(/https:\/\/docs\.xmlui\.org\/components\//g, 'https://www.xmlui.org/docs/reference/components/')
    .replace(/https:\/\/docs\.xmlui\.org\//g, 'https://www.xmlui.org/docs/');
}

// XMLUI's Markdown sanitizes file:// URLs and rewrites their anchors into
// non-clickable spans, so we can't get a working file-link out of Markdown.
// Strip the image-source footers from the markdown text and return them as
// a separate array; the Transcript component renders them as inline thumbnails
// and Sessions as XMLUI Links.
function extractImagePaths(text) {
  if (!text) return [];
  const paths = [];
  const re = /\[Image: source: (\/[^\]]+\.(?:png|jpg|jpeg|gif|webp))\]/gi;
  let m;
  while ((m = re.exec(text)) !== null) paths.push(m[1]);
  return paths;
}
function stripImagePaths(text) {
  if (!text) return text;
  return text.replace(/\n*\[Image: source: \/[^\]]+\.(?:png|jpg|jpeg|gif|webp)\]/gi, '');
}

// Same shape as extractImagePaths/stripImagePaths but for GitHub-flavored
// markdown: `![alt](url)` and `<img src="url">`. Used by Issues to mirror
// Sessions' thumbnail-with-fullscreen pattern.
function extractMarkdownImages(text) {
  if (!text) return [];
  const urls = [];
  const md = /!\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g;
  let m;
  while ((m = md.exec(text)) !== null) urls.push(m[1]);
  const html = /<img\b[^>]*\bsrc=["']([^"']+)["'][^>]*>/gi;
  while ((m = html.exec(text)) !== null) urls.push(m[1]);
  return urls;
}
function stripMarkdownImages(text) {
  if (!text) return text;
  return text
    .replace(/\n*!\[[^\]]*\]\([^)\s]+(?:\s+"[^"]*")?\)/g, '')
    .replace(/\n*<img\b[^>]*\bsrc=["'][^"']+["'][^>]*>/gi, '');
}

// True when the most recent textful turn in the session is a user turn —
// i.e. the user has spoken (or a worklist button submitted via toTurn) but
// the assistant has not yet emitted text. tool_use-only assistant records
// and tool_result-only user records are skipped so a long tool cycle still
// reads as "waiting". Used by Transcript's "agent is thinking" spinner.
// Return the suffix of jsonlText containing the trailing `n` records.
// Walks backward counting newlines; returns the whole text if there are
// fewer than n records. Used as a stable, small cache key for helpers
// that only need the trailing window of a session JSONL (issue #100
// suffix-keying — keeps re-derivation O(suffix) instead of O(file) as
// sessions grow into the megabytes). With the diff+cap pipeline the
// shared cache is bounded at ~1.5 MB, but most helpers only need the
// last 50-100 records regardless of buffer size.
function _lastNRecords(jsonlText, n) {
  if (!jsonlText || n <= 0) return '';
  let pos = jsonlText.length;
  // Skip a trailing newline so the last non-empty record counts.
  if (pos > 0 && jsonlText.charCodeAt(pos - 1) === 10) pos--;
  let count = 0;
  while (pos > 0) {
    pos--;
    if (jsonlText.charCodeAt(pos) === 10) {
      count++;
      if (count >= n) return jsonlText.substring(pos + 1);
    }
  }
  return jsonlText;
}

function isWaitingForAssistant(jsonlText) {
  if (!jsonlText) return false;
  // Identity fast-path: when this binding fires on every keystroke
  // (TextArea `enabled` prop) lastJsonl is usually unchanged. O(1)
  // identity check beats walking ~100 KB backward to compute the
  // suffix on every keystroke.
  if (isWaitingForAssistant._fullKey === jsonlText) {
    return isWaitingForAssistant._cacheValue;
  }
  const _t0 = App.now();
  // Suffix-keyed (issue #100): the answer depends only on the trailing
  // few records — anything before the most recent text-bearing record
  // is irrelevant. We cache on the suffix string so identical trailing
  // content hits the cache even when the upstream JSONL has grown.
  const suffix = _lastNRecords(jsonlText, 50);
  if (isWaitingForAssistant._cacheKey === suffix) {
    isWaitingForAssistant._fullKey = jsonlText;
    return isWaitingForAssistant._cacheValue;
  }
  const lines = suffix.split('\n');
  let lastRole = null;
  for (const line of lines) {
    if (!line) continue;
    let r;
    try { r = JSON.parse(line); } catch (e) { continue; }
    if (r.type === 'user' && r.message && r.message.content) {
      const content = r.message.content;
      if (Array.isArray(content) && content.length > 0 &&
          content.every(c => c && c.type === 'tool_result')) continue;
      lastRole = 'user';
    } else if (r.type === 'assistant' && r.message && r.message.content) {
      const content = r.message.content;
      if (typeof content === 'string') {
        lastRole = 'assistant';
      } else if (Array.isArray(content) && content.some(c => c && c.type === 'text')) {
        lastRole = 'assistant';
      }
    } else if (r.type === 'event_msg' && r.payload) {
      if (r.payload.type === 'user_message') lastRole = 'user';
      if (r.payload.type === 'agent_message') lastRole = 'assistant';
    }
  }
  const value = (lastRole === 'user');
  isWaitingForAssistant._fullKey = jsonlText;
  isWaitingForAssistant._cacheKey = suffix;
  isWaitingForAssistant._cacheValue = value;
  _traceHelperTiming('isWaitingForAssistant', _t0, { len: jsonlText.length, suffixLen: suffix.length, lines: lines.length });
  return value;
}

// Iframe-side trace helper for the [iframe] category of the comms-path
// trace log (issue #49). Forwards a structured record to the host's
// `log_from_right_pane` Tauri command, which routes records whose
// `kind` is `"iframe-trace"` into resources/bram-traces/bram-trace.log when
// BRAM_TRACE=1 is set on the host. No-op when logToHost isn't wired up.
// subkind is a token from the spec's maintained vocabulary (click,
// inflight-set, inflight-clear, listener-fired, ...); fields are
// arbitrary per-event metadata (target, item, reason, paths, etc.).
function iframeTrace(subkind, fields) {
  try {
    if (typeof logToHost !== 'function') return;
    const payload = { kind: 'iframe-trace', subkind: subkind, at: new Date().toISOString() };
    if (fields && typeof fields === 'object') {
      Object.assign(payload, fields);
    }
    logToHost(payload);
  } catch (e) {}
}

// Cascade-diagnosis instrumentation (refs #93). Emits a helper-call
// record to bram-trace when a hot JSONL-walking helper exceeds the
// threshold. Cheap paths (no-op early returns, cache hits) don't log
// because their _t0 measurement is sub-ms. Threshold deliberately
// low to catch sub-frame stalls that compound across the cascade.
function _traceHelperTiming(name, t0, extra) {
  try {
    const _elapsed = App.now() - t0;
    if (_elapsed < 2) return;
    if (typeof logToHost !== 'function') return;
    const payload = { kind: 'iframe-trace', subkind: 'helper-call', name: name, ms: Math.round(_elapsed), at: new Date().toISOString() };
    if (extra && typeof extra === 'object') Object.assign(payload, extra);
    logToHost(payload);
  } catch (e) {}
}

// Clean a user turn for transcript display: strip protocol prefixes
// (`voice: `, `talk: `) so spoken / typed content reads as plain text;
// summarize structured Worklist lifecycle payloads to a one-line
// glyph + count instead of dumping JSON. Anything else passes through.
function formatUserTurnForTranscript(text) {
  if (!text) return '';
  const stripped = text.replace(/^(voice|talk):\s*/, '');
  if (stripped !== text) return stripped;
  const m = text.match(/^(approved|drop|iterate):\s*(.*)$/s);
  if (m) {
    const kind = m[1];
    try {
      const data = JSON.parse(m[2]);
      const n = ((data.items || data.ids || [])).length;
      if (kind === 'approved') {
        return '✓ Approved ' + n + ' item' + (n === 1 ? '' : 's');
      }
      if (kind === 'iterate') {
        return 'Iterated ' + n + ' item' + (n === 1 ? '' : 's');
      }
      return '✕ Dropped ' + n + ' item' + (n === 1 ? '' : 's');
    } catch (e) {
      return text;
    }
  }
  return text;
}

// Compact one-line summary for a tool_use block. Falls back to the tool
// name when the input shape is unfamiliar.
function toolSummary(name, input) {
  if (!input || typeof input !== 'object') return name || '';
  if (name === 'Edit' || name === 'MultiEdit') {
    return (input.file_path || '') + ' edited';
  }
  if (name === 'Write') {
    const lines = (input.content || '').split('\n').length;
    return (input.file_path || '') + ' — wrote ' + lines + ' line' + (lines === 1 ? '' : 's');
  }
  if (name === 'Bash') {
    const cmd = input.command || '';
    return cmd.length > 80 ? cmd.slice(0, 80) + '…' : cmd;
  }
  if (name === 'Read') {
    let s = input.file_path || '';
    if (input.offset || input.limit) {
      const start = input.offset || 1;
      s += ':' + start;
      if (input.limit) s += '-' + (start + input.limit - 1);
    }
    return s;
  }
  if (name === 'Grep' || name === 'Glob') {
    return (input.pattern || '') + (input.path ? ' in ' + input.path : '');
  }
  if (name === 'Task' || name === 'Agent') {
    return (input.subagent_type || '') + (input.description ? ' — ' + input.description : '');
  }
  return name || '';
}

function parseJsonString(value) {
  if (typeof value !== 'string') return null;
  try {
    return JSON.parse(value);
  } catch (e) {
    return null;
  }
}

function codexToolName(payload) {
  if (!payload) return '';
  if (payload.namespace) return payload.namespace.replace(/^mcp__/, '') + '.' + (payload.name || '');
  return payload.name || '';
}

function codexToolInput(payload) {
  if (!payload) return {};
  if (payload.type === 'function_call') {
    const parsed = parseJsonString(payload.arguments);
    return parsed !== null ? parsed : (payload.arguments || {});
  }
  if (payload.type === 'custom_tool_call') {
    const parsed = parseJsonString(payload.input);
    return parsed !== null ? parsed : (payload.input || '');
  }
  return {};
}

function codexToolSummary(payload) {
  if (!payload) return '';
  const name = codexToolName(payload);
  const input = codexToolInput(payload);
  if (payload.name === 'exec_command' && input && typeof input === 'object' && input.cmd) {
    return input.cmd.length > 80 ? input.cmd.slice(0, 80) + '…' : input.cmd;
  }
  if (payload.name === 'write_stdin' && input && typeof input === 'object') {
    const chars = input.chars || '';
    const session = input.session_id ? ('session ' + input.session_id) : 'stdin';
    if (!chars) return session;
    const label = chars === '\u001b' ? 'Esc' : chars.replace(/\r/g, '\\r').replace(/\n/g, '\\n');
    return session + ' ← ' + (label.length > 40 ? label.slice(0, 40) + '…' : label);
  }
  if (payload.name === 'apply_patch' && typeof input === 'string') {
    const m = input.match(/\*\*\* (?:Add|Update|Delete) File: ([^\n]+)/);
    return m ? (m[1] + ' patch') : 'patch';
  }
  if (name.startsWith('filesystem.') && input && typeof input === 'object' && input.path) {
    return input.path;
  }
  if (name.startsWith('xmlui.') && input && typeof input === 'object') {
    return input.path || input.component || input.query || name;
  }
  if (input && typeof input === 'object') return toolSummary(payload.name || name, input);
  return name;
}

// First N lines of a Write tool's content, plus the leftover count for
// the truncation footer.
function writeBodyLines(input, maxLines) {
  const cap = maxLines || 20;
  if (!input || !input.content) return { lines: [], remaining: 0 };
  const all = input.content.split('\n');
  return { lines: all.slice(0, cap), remaining: Math.max(0, all.length - cap) };
}

// Pretty-print arbitrary tool input as JSON, truncated to N lines.
function toolInputJsonLines(input, maxLines) {
  const cap = maxLines || 20;
  if (input === null || input === undefined) return { lines: [], remaining: 0 };
  if (typeof input === 'string') {
    const all = input.split('\n');
    return { lines: all.slice(0, cap), remaining: Math.max(0, all.length - cap) };
  }
  let json;
  try {
    json = JSON.stringify(input, null, 2);
  } catch (e) {
    return { lines: ['(unserializable input)'], remaining: 0 };
  }
  const all = json.split('\n');
  return { lines: all.slice(0, cap), remaining: Math.max(0, all.length - cap) };
}

// Concatenate the text content of a tool_result block (handles both
// string and array-of-blocks shapes).
function toolResultText(content) {
  if (!content) return '';
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) {
    return content
      .filter(c => c && c.type === 'text')
      .map(c => c.text || '')
      .join('\n');
  }
  return '';
}

// True if a tool_result block carries an error (either flagged via
// is_error or detected by an Error:/<tool_use_error> prefix). Used to
// tint the inline result banner red.
function isErrorResult(block) {
  if (!block) return false;
  if (block.is_error) return true;
  const text = toolResultText(block.content);
  return text.startsWith('Error:') || text.startsWith('<tool_use_error>');
}

function codexToolOutput(payload) {
  if (!payload || (payload.type !== 'function_call_output' && payload.type !== 'custom_tool_call_output')) {
    return null;
  }
  const raw = payload.output;
  if (typeof raw !== 'string') return { text: '', errored: false };
  const parsed = parseJsonString(raw);
  if (parsed && typeof parsed === 'object') {
    const text = typeof parsed.output === 'string'
      ? parsed.output
      : typeof parsed.stderr === 'string'
        ? parsed.stderr
        : raw;
    const exitCode = parsed.metadata && typeof parsed.metadata.exit_code === 'number'
      ? parsed.metadata.exit_code
      : null;
    return { text, errored: exitCode !== null && exitCode !== 0 };
  }
  const exitMatch = raw.match(/Process exited with code (\d+)/);
  const exitCode = exitMatch ? parseInt(exitMatch[1], 10) : 0;
  return { text: raw, errored: !!exitMatch && exitCode !== 0 };
}

// Find a tool entry by id across all turns. Used by Transcript to render the
// open-tool modal — we keep `openToolId` as the source of truth and
// derive the entry from it on each render.
function findToolInTurns(turns, toolId) {
  if (!turns || !toolId) return null;
  for (const t of turns) {
    if (!t || !t.entries) continue;
    for (const e of t.entries) {
      if (e && e.kind === 'tool' && e.id === toolId) return e;
    }
  }
  return null;
}

// Shallow turn equality: enough to tell "unchanged turn" from
// "changed/new" without doing a full JSON.stringify. Used by sessionTurns
// to preserve object refs for stable turns so XMLUI's Items doesn't
// re-mount the whole list on every poll.
function turnsLooselyEqual(a, b) {
  if (!a || !b) return false;
  if (a.role !== b.role) return false;
  if (a.text !== b.text) return false;
  const ae = a.entries || [], be = b.entries || [];
  if (ae.length !== be.length) return false;
  for (let i = 0; i < ae.length; i++) {
    const x = ae[i], y = be[i];
    if (!x || !y) return false;
    if (x.kind !== y.kind) return false;
    if (x.kind === 'text') {
      if (x.text !== y.text) return false;
    } else {
      // tool: id is stable, errored/result may change between polls
      if (x.id !== y.id) return false;
      if (!!x.errored !== !!y.errored) return false;
    }
  }
  const ai = a.images || [], bi = b.images || [];
  if (ai.length !== bi.length) return false;
  return true;
}

// Return the last N turns, reusing the previous result by reference when
// every visible turn is still the same object. `sessionTurns` already
// preserves stable refs across polls, so on a steady-state idle session
// every element of `prev` and `cur` matches and we hand back the same
// array — XMLUI's Items can then skip remounting the visible list.
function visibleTurns(turns, n) {
  if (!turns || !n) return visibleTurns._cacheValue || [];
  const start = Math.max(0, turns.length - n);
  const prev = visibleTurns._cacheValue;
  if (prev && prev.length === turns.length - start) {
    let same = true;
    for (let i = 0; i < prev.length; i++) {
      if (prev[i] !== turns[start + i]) { same = false; break; }
    }
    if (same) return prev;
  }
  const out = turns.slice(start);
  visibleTurns._cacheValue = out;
  return out;
}

// Parse a slice of JSONL lines into the turn-list shape sessionTurns
// returns. `toolIndex` (optional) lets the caller pre-populate the
// tool_use_id → entry map so cross-boundary tool_results in an
// incremental parse can still find their originating tool. Returns
// only the turns generated from `lines` (no structural-share — that's
// the caller's responsibility). Extracted from sessionTurns so the
// full-parse and incremental paths share it (issue #100).
function _parseLinesToTurns(lines, toolIndex) {
  toolIndex = toolIndex || {};
  const turns = [];
  for (const line of lines) {
    if (!line) continue;
    let r;
    try { r = JSON.parse(line); } catch (e) { continue; }
    let role = null;
    const entries = [];
    const inlineImages = [];
    if (r.type === 'user' || r.type === 'assistant') {
      if (!r.message || !r.message.content) continue;
      role = r.type;
      const content = r.message.content;
      if (typeof content === 'string') {
        if (content) entries.push({ kind: 'text', text: content });
      } else if (Array.isArray(content)) {
        for (const c of content) {
          if (!c) continue;
          if (c.type === 'text' && c.text) {
            entries.push({ kind: 'text', text: c.text });
          } else if (c.type === 'tool_use') {
            // Keep entries lightweight — only what the collapsed row
            // needs. Full input/result are fetched on expand via
            // getToolDetail.
            const entry = {
              kind: 'tool',
              id: c.id,
              name: c.name,
              summary: toolSummary(c.name, c.input || {}),
            };
            entries.push(entry);
            if (c.id) toolIndex[c.id] = entry;
          } else if (c.type === 'tool_result') {
            const matching = c.tool_use_id && toolIndex[c.tool_use_id];
            if (matching) {
              matching.errored = isErrorResult(c);
              if (matching.errored) {
                const txt = toolResultText(c.content);
                matching.errorText = txt.split('\n')[0].slice(0, 200);
              }
            }
          } else if (c.type === 'image' && c.source && c.source.type === 'base64' && c.source.data) {
            const mt = c.source.media_type || 'image/png';
            inlineImages.push('data:' + mt + ';base64,' + c.source.data);
          }
        }
      }
    } else if (r.type === 'event_msg' && r.payload) {
      if (r.payload.type === 'user_message') role = 'user';
      if (r.payload.type === 'agent_message') role = 'assistant';
      const t = r.payload.message || '';
      if (t) entries.push({ kind: 'text', text: t });
    } else if (r.type === 'response_item' && r.payload) {
      const p = r.payload;
      if (p.type === 'function_call' || p.type === 'custom_tool_call') {
        role = 'assistant';
        const entry = {
          kind: 'tool',
          id: p.call_id,
          name: codexToolName(p),
          summary: codexToolSummary(p),
        };
        entries.push(entry);
        if (p.call_id) toolIndex[p.call_id] = entry;
      } else if (p.type === 'function_call_output' || p.type === 'custom_tool_call_output') {
        const matching = p.call_id && toolIndex[p.call_id];
        if (matching) {
          const output = codexToolOutput(p);
          matching.errored = !!(output && output.errored);
          if (output && output.text) {
            const firstLine = output.text.split('\n')[0].slice(0, 200);
            if (matching.errored) matching.errorText = firstLine;
          }
        }
      }
    }
    if (!role) continue;
    if (entries.length === 0 && inlineImages.length === 0) continue;
    // Capture image paths from the ORIGINAL text before stripping — strip
    // and extract operate on the same patterns, so we have to read before
    // we clean. (Was previously running extract on already-stripped text,
    // which made the [Image: source: ...] fallback dead code.)
    const originalJoined = entries.filter(e => e.kind === 'text').map(e => e.text).join('\n\n');
    const pathsFromText = extractImagePaths(originalJoined);
    // Apply text rewrites + strip image-path footers from text entries.
    for (const e of entries) {
      if (e.kind === 'text') {
        e.text = stripImagePaths(rewriteXmluiDocUrls(e.text));
      }
    }
    const textJoined = entries.filter(e => e.kind === 'text').map(e => e.text).join('\n\n');
    // Skip user turns that are pure image-path bookkeeping (preserved from prior behavior).
    if (role === 'user' && inlineImages.length === 0 && entries.every(e => e.kind === 'text')
        && /^(\[Image: source: [^\]]+\]\s*)+$/.test(originalJoined.trim())) continue;
    // After tool_result filtering, a user turn may have nothing left.
    if (entries.length === 0 && inlineImages.length === 0) continue;
    turns.push({
      role,
      text: textJoined,
      entries,
      images: inlineImages.length > 0 ? inlineImages : pathsFromText,
    });
  }
  return turns;
}

function sessionTurns(jsonlText) {
  // Sticky empty: during a refetch the DataSource value can briefly be
  // null/undefined. Returning [] would blank the transcript and cause a
  // dramatic flash; instead, hold the last result until the new value
  // arrives.
  if (!jsonlText) return sessionTurns._cacheValue || [];
  // Function-property memoization: skip the reparse when the polled
  // JSONL hasn't changed since last call. Identity comparison is enough
  // because the DataSource hands us a fresh string only when the file
  // actually grew.
  if (sessionTurns._cacheKey === jsonlText && sessionTurns._cacheValue) {
    return sessionTurns._cacheValue;
  }

  // Incremental parse (issue #100): if the prior cacheKey is a strict
  // prefix of the new jsonlText, parse only the suffix and concat onto
  // the prior turns. Existing turn objects are reused by reference so
  // XMLUI's reactivity sees only the new turns as changed. Works
  // because the diff-based latest-tail (issue #100) hands us
  // append-only growth between cap-trims. The cap-trim case breaks
  // the prefix property (new buffer is a suffix of old, not a prefix),
  // which falls through to full parse below — correct, just costly on
  // that one tick.
  const prevKey = sessionTurns._cacheKey;
  const prevValue = sessionTurns._cacheValue;
  if (prevKey && prevValue && jsonlText.length > prevKey.length &&
      jsonlText.substring(0, prevKey.length) === prevKey) {
    const _t0 = App.now();
    const suffix = jsonlText.substring(prevKey.length);
    // Pre-populate toolIndex from prior tool entries so suffix
    // tool_results can locate their originating tool_use.
    const toolIndex = {};
    for (const t of prevValue) {
      for (const e of (t.entries || [])) {
        if (e && e.kind === 'tool' && e.id) toolIndex[e.id] = e;
      }
    }
    const newTurns = _parseLinesToTurns(suffix.split('\n'), toolIndex);
    sessionTurns._cacheKey = jsonlText;
    sessionTurns._cacheValue = newTurns.length > 0
      ? prevValue.concat(newTurns)
      : prevValue;
    sessionTurns._parseCount = (sessionTurns._parseCount || 0) + 1;
    const _elapsed = App.now() - _t0;
    if (_elapsed > 2 || newTurns.length > 0) {
      iframeTrace('sessionTurns-parse', {
        ms: Math.round(_elapsed),
        len: jsonlText.length,
        suffixLen: suffix.length,
        turns: sessionTurns._cacheValue.length,
        newTurns: newTurns.length,
        n: sessionTurns._parseCount,
        path: 'incremental',
      });
    }
    return sessionTurns._cacheValue;
  }

  // Full-parse fallback: no prior cache, or new jsonlText doesn't
  // extend the prior key (session rotation, cap-trim head-drop, etc.).
  // Instrumentation: log cache-miss parses. Tracks how often we do real
  // work and how long it takes. App.now is the xmlui-native managed
  // replacement for performance.now (banned under strictDomSandbox).
  const _t0 = App.now();
  sessionTurns._parseCount = (sessionTurns._parseCount || 0) + 1;
  const turns = _parseLinesToTurns(jsonlText.split('\n'));
  // Structural-share with the previous result: for each turn that's
  // structurally equal to the previous turn at the same index, reuse
  // the previous reference. XMLUI's reactivity treats reference
  // equality as "unchanged", so the Items in Transcript skips re-mounting
  // those turns — eliminating the per-poll flash. JSONL is append-only
  // in practice, so the first N-K turns are typically identical and
  // only the last few are new or growing.
  const prev = sessionTurns._cacheValue || [];
  for (let i = 0; i < turns.length && i < prev.length; i++) {
    if (turnsLooselyEqual(turns[i], prev[i])) {
      turns[i] = prev[i];
    } else {
      break;
    }
  }
  sessionTurns._cacheKey = jsonlText;
  sessionTurns._cacheValue = turns;
  const _elapsed = App.now() - _t0;
  if (_elapsed > 2) {
    iframeTrace('sessionTurns-parse', {
      ms: Math.round(_elapsed),
      len: jsonlText.length,
      turns: turns.length,
      n: sessionTurns._parseCount,
      path: 'full',
    });
  }
  return turns;
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
  App.mark('iterate-enabled');
  return !submitting && !!selected && (selectedFeedback || '').trim().length > 0;
}

function traceApproveDropEnabled(submitting, selected) {
  App.mark('approve-drop-enabled');
  return !submitting && !!selected;
}

function buildApprovePayload(items, selectedId, feedback) {
  App.mark('build-approve-payload');
  return JSON.stringify({
    items: (items || []).filter(function (i) { return i.id === selectedId; })
      .map(function (i) { return { id: i.id, hash: i.hash, feedback: feedback }; })
  });
}

function buildIteratePayload(items, selectedId, feedback) {
  App.mark('build-iterate-payload');
  // feedback may be either an inline string (backward-compat, used by
  // any caller that hasn't migrated to the backing-store flow yet) or
  // a `{ feedbackRef: "<id>" }` object (new, used by the Iterate click
  // after queueFeedbackDraft has written the draft to disk). See #144.
  return JSON.stringify({
    items: (items || []).filter(function (i) { return i.id === selectedId; })
      .map(function (i) {
        return feedback && typeof feedback === 'object' && feedback.feedbackRef
          ? { id: i.id, hash: i.hash, feedbackRef: feedback.feedbackRef }
          : { id: i.id, hash: i.hash, feedback: feedback };
      })
  });
}

function buildDropPayload(items, selectedId, feedback) {
  App.mark('build-drop-payload');
  return JSON.stringify({
    items: (items || []).filter(function (i) { return i.id === selectedId; })
      .map(function (i) { return { id: i.id, hash: i.hash, feedback: feedback }; })
  });
}

function buildSingleItemApprovePayload(itemRef, feedback) {
  App.mark('build-single-item-approve-payload');
  return JSON.stringify({
    items: [{ id: itemRef.id, hash: itemRef.hash, feedback: feedback }]
  });
}

// Batch actions (issue #97): one approved:/drop: payload over every
// item in a status group. Scoped to 'applied' (TO COMMIT) — see the
// Approve all / Drop all bar in Workspace.xmlui.
function countByStatus(items, status) {
  return (items || []).filter(function (i) { return (i.status || 'proposed') === status; }).length;
}

function buildBatchApprovePayload(items, feedback) {
  App.mark('build-batch-approve-payload');
  return JSON.stringify({
    items: (items || []).filter(function (i) { return (i.status || 'proposed') === 'applied'; })
      .map(function (i) { return { id: i.id, hash: i.hash, feedback: feedback || '' }; })
  });
}

function buildBatchDropPayload(items, feedback) {
  App.mark('build-batch-drop-payload');
  return JSON.stringify({
    items: (items || []).filter(function (i) { return (i.status || 'proposed') === 'applied'; })
      .map(function (i) { return { id: i.id, hash: i.hash, feedback: feedback || '' }; })
  });
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

// Feedback tab helpers — parallel to the history* family. The Feedback
// tab browses entries from /__feedback-history/list, each shaped as
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

function restoreWorklistDraft() {
  return readLocalStorage('bram.worklistMessageDraft', '');
}

function persistWorklistDraftCursor(component) {
  if (!component || typeof component.selectionStart !== 'number') return;
  writeLocalStorage('bram.worklistMessageDraftCursor', JSON.stringify({
    start: component.selectionStart,
    end: typeof component.selectionEnd === 'number'
      ? component.selectionEnd
      : component.selectionStart,
    direction: component.selectionDirection || 'none'
  }));
}

function restoreWorklistDraftCursor(component) {
  if (!component || typeof component.setSelectionRange !== 'function') return false;
  const raw = readLocalStorage('bram.worklistMessageDraftCursor', '');
  if (!raw) return false;
  let saved;
  try { saved = JSON.parse(raw); } catch (e) { return false; }
  if (!saved || typeof saved.start !== 'number') return false;
  const currentLength = String(component.value || '').length;
  return restoreTextSelection(component, saved, currentLength, 0);
}

// Recency-gated restore of the awaiting gate so hot-reloading the
// iframe in the middle of a real turn doesn't drop the user back to
// the stale prior agent text. Window deliberately generous (5 min)
// to cover long agent turns; out-of-window entries get cleared so
// a long-dead Bram session can't sticky-lock the gate.
function restoreWorklistAwaiting() {
  const flag = readLocalStorage('bram.awaitingResponse', '');
  const setAtRaw = readLocalStorage('bram.awaitingResponseSetAt', '');
  const setAt = parseInt(setAtRaw, 10);
  if (flag === '1' && !isNaN(setAt) && (Date.now() - setAt) < 300000) {
    return true;
  }
  writeLocalStorage('bram.awaitingResponse', '');
  writeLocalStorage('bram.awaitingResponseSetAt', '');
  return false;
}

function restoreWorklistAwaitingSetAt() {
  const setAtRaw = readLocalStorage('bram.awaitingResponseSetAt', '');
  const setAt = parseInt(setAtRaw, 10);
  return isNaN(setAt) ? 0 : setAt;
}

// Write the awaiting-started state to localStorage so a hot-reload
// can re-hydrate it within the recency window. Returns the timestamp
// it wrote so call sites can sync the in-memory awaitingResponseSetAt
// var to the persisted value.
function markAwaitingStarted() {
  const now = Date.now();
  writeLocalStorage('bram.awaitingResponse', '1');
  writeLocalStorage('bram.awaitingResponseSetAt', String(now));
  return now;
}

function restoreWorklistSubmittedMessage() {
  return readLocalStorage('bram.worklistSubmittedMessage', '');
}

function restoreWorklistSubmittedBaseline() {
  const raw = readLocalStorage('bram.worklistSubmittedBaseline', '');
  const n = parseInt(raw, 10);
  return isNaN(n) ? 0 : n;
}

function submitWorklistMessage(text, baseline) {
  if (!text || !text.trim()) return false;
  const message = text.trim();
  const sentAt = Date.now();
  iframeTrace('message-agent-submit', { stage: 'before-toTurn', chars: message.length, sentAt });
  toTurn(message);
  iframeTrace('message-agent-submit', { stage: 'after-toTurn', chars: message.length, sentAt });
  writeLocalStorage('bram.awaitingResponse', '');
  writeLocalStorage('bram.worklistMessageDraft', '');
  writeLocalStorage('bram.worklistMessageDraftCursor', '');
  writeLocalStorage('bram.worklistSubmittedMessage', message);
  writeLocalStorage('bram.worklistSubmittedBaseline', String(baseline || 0));
  return message;
}

function submitWorklistMessageFast(text) {
  if (!text || !text.trim()) return false;
  const userTyped = text.trim();
  const toSend = withStagedImageMarkers(userTyped);
  const sentAt = Date.now();
  iframeTrace('message-agent-submit', { stage: 'before-toTurn', chars: toSend.length, sentAt });
  toTurn(toSend);
  iframeTrace('message-agent-submit', { stage: 'after-toTurn', chars: toSend.length, sentAt });
  const baseline = 0;
  writeLocalStorage('bram.worklistMessageDraft', '');
  writeLocalStorage('bram.worklistMessageDraftCursor', '');
  // Track the user-typed text (not the marker-augmented send), so it matches
  // the stripped `userText` the JSONL extractor returns and
  // `conversationExchangeMatchesSubmitted` resolves true. Mismatch keeps
  // awaitingResponse stuck (it clears on exchange-match) and gates
  // conversationUserImages to [] during awaiting.
  writeLocalStorage('bram.worklistSubmittedMessage', userTyped);
  writeLocalStorage('bram.worklistSubmittedBaseline', String(baseline || 0));
  return { message: userTyped, baseline, sentAtText: new Date().toLocaleTimeString() };
}

// Drain any clipboard-staged image paths and prepend the dual marker format
// captureScreenshot also uses: `@<path>` triggers claude-code's Read tool,
// `[Image: source: <path>]` is what st_extract_image_paths matches for the
// thumbnail strip in the conversation pane.
function withStagedImageMarkers(text) {
  const paths = window.bramConsumePastedImagePaths
    ? window.bramConsumePastedImagePaths()
    : [];
  if (!paths || paths.length === 0) return text;
  const lines = paths.map(p => 'Read this screenshot: @' + p + '\n[Image: source: ' + p + ']');
  const markers = lines.join('\n');
  return text ? markers + '\n\n' + text : markers;
}

function recordWorklistFeedbackConversation(text) {
  if (!text || !text.trim()) return false;
  const message = text.trim();
  const baseline = 0;
  writeLocalStorage('bram.worklistSubmittedMessage', message);
  writeLocalStorage('bram.worklistSubmittedBaseline', String(baseline));
  return { message, baseline, sentAtText: new Date().toLocaleTimeString() };
}

function clearWorklistAwaiting(clearDraft) {
  writeLocalStorage('bram.awaitingResponse', '');
  writeLocalStorage('bram.awaitingResponseSetAt', '');
  if (clearDraft) {
    writeLocalStorage('bram.worklistMessageDraft', '');
  }
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
  if (kind === 'approved') return 'Approving';
  if (kind === 'iterate') return 'Iterating';
  if (kind === 'drop') return 'Dropping';
  return '';
}

// Strip the screenshot-paste marker prefix that submitWorklistMessageFast
// prepends to outgoing messages. The JSONL parser removes `[Image: source: ...]`
// but leaves the `Read this screenshot: @<path>` prefix in userText (claude-code
// needs the `@<path>` to fire its Read tool). For exchange-match purposes we
// reduce both sides to just the user-typed text.
function stripImageMarkerPrefix(text) {
  return (text || '').replace(/^(\s*Read this screenshot: @\S+\s*)+/, '').trim();
}

function worklistSubmittedMatches(exchangeUserText, submitted) {
  if (!submitted) return false;
  const norm = s => stripImageMarkerPrefix(s || '').replace(/\s+/g, ' ').trim();
  return norm(exchangeUserText) === norm(submitted);
}

function worklistConversationUserText(exchangeUserText, submitted, awaiting, submittedItemId) {
  App.mark('worklist:conv-user-text:start');
  // Strip the `Read this screenshot: @<path>` marker prefix that
  // submitWorklistMessageFast prepended on the way out. The JSONL extractor
  // already removes `[Image: source: ...]` but leaves the `@<path>` prefix
  // in userText. Without this strip, the conversation pane's "You" element
  // flaps between the marker-augmented exchange text (long) and the typed
  // submittedWorklistMessage (short) as lastExchangeDS refetches return
  // empty mid-cycle. Stripping unifies both branches to the typed text.
  const exchange = stripImageMarkerPrefix(String(exchangeUserText || '')).trim();
  const display = String(submitted || '').trim();
  const exchangeIsPayload = isWorklistActionPayloadText(exchange);
  let result;
  let branch;
  if (awaiting && display) {
    result = display;
    branch = 'awaiting+display';
  } else if (exchangeIsPayload) {
    if (display) {
      result = display;
      branch = 'payload+display';
    } else {
      result = formatUserTurnForTranscript(exchange);
      branch = 'payload+summary';
    }
  } else {
    result = (exchange || display).trim();
    branch = 'fallback';
  }
  App.mark('worklist:conv-user-text:branch=' + branch +
    '|awaiting=' + (awaiting ? '1' : '0') +
    '|isPayload=' + (exchangeIsPayload ? '1' : '0') +
    '|exchangeStart=' + exchange.slice(0, 16).replace(/\|/g, '_') +
    '|displayStart=' + display.slice(0, 16).replace(/\|/g, '_') +
    '|resultStart=' + (result || '').slice(0, 40).replace(/\|/g, '_'));
  App.measure('worklist:conv-user-text:dur', 'worklist:conv-user-text:start');
  return result;
}

// Returns true when the agent-turn-killed event should actually clear
// awaitingResponse. Returns false (and emits the suppression trace) when
// the kill arrived within the grace window — the host emits `agent-turn-
// killed` immediately when a structured iterate/approve/drop/new-item
// payload "kills" the prior idle turn, which would otherwise flip
// awaitingResponse off and reveal the stale prior agent text via the
// fallback chain. Legitimate user-triggered kills come much later.
function shouldClearOnAgentTurnKilled(awaitingResponseSetAt, exchangeUserText, submittedText) {
  const submitted = (submittedText || '').trim();
  if (submitted && !worklistSubmittedMatches(exchangeUserText, submitted)) {
    iframeTrace('awaiting-kill-suppressed', { reason: 'exchange-stale' });
    return false;
  }
  const sinceSet = Date.now() - (awaitingResponseSetAt || 0);
  if (sinceSet > 750) {
    return true;
  }
  iframeTrace('awaiting-kill-suppressed', { reason: 'within-window', sinceSet });
  return false;
}

// Per-tab splitter persistence. XMLUI's HSplitter `onResize` actually
// delivers `[primary, secondary]` as a two-element array (the docs at
// https://docs.xmlui.org/components/Splitter claim `primarySize:
// number`, but the trace at restore-window-and-splitter-state showed
// an array). The unit is inconsistent: the first event of a drag
// arrives in pixels (e.g. `[491, 491]` for a 982 px container) and
// subsequent events arrive in percentages summing to 100. Normalize
// both forms to `primary / (primary + secondary) * 100` percent.
// Store as a percent string so `initialPrimarySize` can rehydrate via
// `'<n>%'`. Note: `writeLocalStorage('bram.splitter.<key>', v)` does
// persist to native localStorage, but XMLUI nests dotted keys under
// the top-level — value lands at `localStorage.bram.splitter.<key>`
// inside the JSON blob at `localStorage['bram']`, not as a flat
// `localStorage['bram.splitter.<key>']` entry. A flat-key sqlite
// probe will miss it; check the `bram` top-level instead.
// Keys: `bram.splitter.<key>` (worklist, sessions, commits, context,
// issues). The outer-shell `bram.splitter.shell` key is owned by
// app/main.js and uses native localStorage flat keys.
function restoreSplitterSize(key, fallback) {
  const raw = readLocalStorage('bram.splitter.' + key, '');
  const n = parseFloat(raw);
  const result = (!isNaN(n) && n > 0 && n < 100) ? (n + '%') : fallback;
  iframeTrace('splitter-restore', { key, raw, result });
  return result;
}
function saveSplitterSize(key, sizes) {
  const a = Array.isArray(sizes) ? sizes[0] : sizes;
  const b = Array.isArray(sizes) ? sizes[1] : 0;
  const total = a + b;
  const pct = total > 0 ? (a / total) * 100 : a;
  iframeTrace('splitter-save', { key, sizes, pct });
  if (pct > 0 && pct < 100) {
    writeLocalStorage('bram.splitter.' + key, String(Math.round(pct * 10) / 10));
  }
}

var worklistVoiceTarget = '';
var worklistVoiceText = '';
var worklistVoiceSeq = 0;

function isWorklistTextVoiceTarget(target) {
  return [
    'message-agent',
    'feedback',
    'new-item',
    'new-issue'
  ].includes(target || '');
}

function setWorklistVoiceTarget(target) {
  const next = target || '';
  if (worklistVoiceTarget === next) return;
  worklistVoiceTarget = next;
  iframeTrace('voice-input', { target: worklistVoiceTarget || 'terminal', stage: 'target' });
}

function restoreTextSelection(control, selection, currentLength, appendedLength) {
  if (!control || !selection) return false;
  const atEnd = selection.start === currentLength && selection.end === currentLength;
  const start = atEnd ? selection.start + appendedLength : selection.start;
  const end = atEnd ? selection.end + appendedLength : selection.end;
  control.setSelectionRange(start, end, selection.direction || 'none');
  return true;
}

function appendVoiceTranscript(component, transcript) {
  if (!component || !transcript) return false;
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
function setToolbarPendingMenuFromEvent(e) {
  recordToolbarPendingMenuFromEvent(e);
}

function traceToolbarKey(key) {
  const toolbarMenuState = getToolbarPendingMenuState();
  iframeTrace('toolbar-key', {
    key,
    menuPresent: toolbarMenuState.present ? 1 : 0,
    menuAgeMs: toolbarMenuState.atMs
      ? (Date.now() - toolbarMenuState.atMs)
      : -1
  });
}

function toggleVoiceForCurrentTarget(recording) {
  if (recording) {
    voiceStop(t => {
      if (!t) return;
      if (isWorklistTextVoiceTarget(worklistVoiceTarget)) {
        worklistVoiceText = t;
        worklistVoiceSeq = worklistVoiceSeq + 1;
        iframeTrace('voice-input', { target: worklistVoiceTarget || 'message-agent', stage: 'stop' });
      } else {
        iframeTrace('voice-input', { target: worklistVoiceTarget || 'terminal', stage: 'fallback-terminal' });
        toTurn('voice: ' + t);
      }
    });
    return false;
  }
  iframeTrace('voice-input', { target: worklistVoiceTarget || 'terminal', stage: 'start' });
  voiceStart();
  return true;
}

function worklistSubmittedAssistant(exchange, submittedMessage) {
  const submitted = worklistMessageKey(submittedMessage);
  if (!submitted || !exchange) return '';
  const userText = worklistMessageKey((exchange && exchange.userText) || '');
  if (userText !== submitted) return '';
  return ((exchange && exchange.assistantText) || '').trim();
}

function worklistConversationSource(turnEntries, latestAssistantText, exchange, submittedMessage, awaiting, baseline) {
  if (turnEntries && turnEntries.length > 0) return 'session-turns';
  if (worklistShouldShowSubmitted([], submittedMessage, awaiting, baseline)) return 'awaiting';
  if (String(latestAssistantText || '').trim()) return 'last-assistant-text';
  if (worklistSubmittedAssistant(exchange, submittedMessage)) return 'last-exchange';
  return 'none';
}

function worklistAssistantFallbackText(turnEntries, latestAssistantText, exchange, submittedMessage, awaiting, baseline) {
  if (turnEntries && turnEntries.length > 0) return '';
  if (worklistShouldShowSubmitted([], submittedMessage, awaiting, baseline)) return '';
  const latest = String(latestAssistantText || '').trim();
  if (latest) return latest;
  return worklistSubmittedAssistant(exchange, submittedMessage);
}

function worklistMessageKey(text) {
  return String(text || '').replace(/\s+/g, ' ').trim();
}

function worklistTurnText(turn) {
  if (!turn) return '';
  const entries = turn.entries || [];
  const parts = [];
  for (let i = 0; i < entries.length; i++) {
    const entry = entries[i];
    if (entry && entry.kind === 'text' && entry.text) {
      parts.push(entry.text);
    }
  }
  return parts.join('\n');
}

function worklistSubmittedAgentEntries(turns, submittedMessage) {
  const submitted = worklistMessageKey(submittedMessage);
  if (!submitted || !turns || !turns.length) return [];
  for (let i = turns.length - 1; i >= 0; i--) {
    const turn = turns[i];
    if (!turn || turn.role !== 'user') continue;
    if (worklistMessageKey(worklistTurnText(turn)) !== submitted) continue;
    return worklistAgentEntriesAfterUser(turns, i);
  }
  return [];
}

function worklistAgentEntriesAfterUser(turns, userIndex) {
  const entries = [];
  const seenToolIds = {};
  let lastTextKey = '';
  if (!turns || userIndex < 0) return entries;
  for (let i = userIndex + 1; i < turns.length; i++) {
    const turn = turns[i];
    if (!turn) continue;
    if (turn.role === 'user') break;
    if (turn.role !== 'assistant') continue;
    const turnEntries = turn.entries || [];
    for (let j = 0; j < turnEntries.length; j++) {
      const entry = turnEntries[j];
      if (!entry) continue;
      if (entry.kind === 'text') {
        const textKey = worklistMessageKey(entry.text || '');
        if (!textKey || textKey === lastTextKey) continue;
        lastTextKey = textKey;
        entries.push(entry);
        continue;
      }
      lastTextKey = '';
      if (entry.kind === 'tool' && entry.id) {
        if (seenToolIds[entry.id]) continue;
        seenToolIds[entry.id] = true;
      }
      entries.push(entry);
    }
  }
  return entries;
}

const COMPACTION_SUMMARY_PREFIX =
  'This session is being continued from a previous conversation that ran out of context.';

function isCompactionSyntheticUserTurn(turn) {
  if (!turn || turn.role !== 'user') return false;
  return worklistTurnText(turn).trim().startsWith(COMPACTION_SUMMARY_PREFIX);
}

function worklistLatestUserIndex(turns) {
  if (!turns || !turns.length) return -1;
  for (let i = turns.length - 1; i >= 0; i--) {
    const turn = turns[i];
    if (turn && turn.role === 'user' && worklistTurnText(turn).trim()) {
      if (isCompactionSyntheticUserTurn(turn)) return -1;
      return i;
    }
  }
  return -1;
}

function worklistLatestUserText(turns) {
  const idx = worklistLatestUserIndex(turns);
  if (idx < 0) return '';
  return worklistTurnText(turns[idx]).trim();
}

// isWorklistActionPayloadText lives in app/__shell/helpers.js as a
// window helper — the xs-script parser choked on regex-literal versions
// here. Bare-name calls resolve via the window scope (same as toTurn,
// logToHost, queueFeedbackDraft).

function worklistLatestAgentEntries(turns) {
  return worklistAgentEntriesAfterUser(turns, worklistLatestUserIndex(turns));
}

function worklistNormalizeForMatch(text) {
  return (text || '').replace(/\s+/g, ' ').trim();
}

function worklistUserTurnCount(turns) {
  if (!turns || !turns.length) return 0;
  let n = 0;
  for (let i = 0; i < turns.length; i++) {
    const turn = turns[i];
    if (turn && turn.role === 'user' && worklistTurnText(turn).trim() && !isCompactionSyntheticUserTurn(turn)) {
      n++;
    }
  }
  return n;
}

function worklistLatestMatchesSubmitted(turns, submittedMessage, baseline) {
  const submitted = worklistNormalizeForMatch(submittedMessage);
  if (!submitted) return false;
  if (worklistUserTurnCount(turns) <= (baseline || 0)) return false;
  const latest = worklistNormalizeForMatch(worklistLatestUserText(turns));
  if (!latest) return false;
  return latest.startsWith(submitted) || submitted.startsWith(latest);
}

function worklistShouldShowSubmitted(turns, submittedMessage, awaiting, baseline) {
  return !!(awaiting && submittedMessage);
}

function worklistDisplayUserText(turns, submittedMessage, awaiting, baseline) {
  if (worklistShouldShowSubmitted(turns, submittedMessage, awaiting, baseline)) {
    return submittedMessage;
  }
  if (submittedMessage) return submittedMessage;
  return worklistLatestUserText(turns) || '';
}

function worklistDisplayAgentEntries(turns, submittedMessage, awaiting, baseline) {
  const submittedEntries = worklistSubmittedAgentEntries(turns, submittedMessage);
  if (submittedEntries.length > 0) return submittedEntries;
  if (submittedMessage && isWorklistActionPayloadText(worklistLatestUserText(turns))) {
    return worklistLatestAgentEntries(turns);
  }
  if (worklistShouldShowSubmitted(turns, submittedMessage, awaiting, baseline)) {
    return [];
  }
  return worklistLatestAgentEntries(turns);
}

// Build the payload object for the pty-menu-changed iframeTrace.
// Inline ternaries inside object literals trip XMLUI's expression
// parser ("Unclosed expression"), so this is split out per rule #9.
function ptyMenuTracePayload(menu) {
  const tool = (menu && menu.tool) || '';
  const hasSig = !!(menu && menu.toolCallSignature);
  const sigChars = hasSig ? menu.toolCallSignature.length : 0;
  return {
    context: 'pty-menu-changed',
    surface: 'worklist',
    tool: tool,
    hasSignature: hasSig,
    signatureChars: sigChars,
  };
}
