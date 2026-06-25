# PTY menu shapes — detection-axis catalog

**What this is:** the shared model of *which interactive menus Bram's PTY
scanner should surface*, and the shape characteristics that identify each.
It turns reactive tuning into a checklist: every cataloged shape must
surface in the agent pane; a miss becomes "shape X regressed" instead of a
guess. Refs #197.

**Detection is grid-primary.** Byte detection has been retired
(`src-tauri/src/lib.rs`: "the grid is now the sole menu detector"). The
iframe reads the xterm grid and reports menus to the host, which builds and
emits the agent-pane menu. The op vocabulary in `bram-trace.log`
(`[grid-menu] op=…`) is now:

- `op=report` — the iframe reported a fresh grid menu (its options).
- `op=build` — host built the menu from a live JSONL tool-use signature
  (carries `tool=<Edit|Write|Bash|…>`).
- `op=build-claude-nosig` — signature-less build of a **Bash** menu from the
  grid (records-stacked / manual-approval / compound cases).
- `op=hold-nosig` — a signature-less frame that is **not** a Bash menu
  (Edit/Write awaiting their signature, or a phantom prose / tool-bullet
  frame); held so the signature path classifies it. See *Signature-less
  discrimination* below.
- `op=override` — grid options corrected a host-detected menu.

The old `op=fire` (byte-scanner era) no longer exists. The byte-scanner axes
below are retained as the **shape vocabulary** (and the historical
`[pty-menu-scan]` diagnostic), not the live detection path.

**Scope = interactive menus Bram can usefully surface**, organized by
*family* — not only tool-permission prompts:

- **Family A — tool-permission prompts** (Claude + Codex). Primary class.
- **Family B — question prompts** (Claude Code *AskUserQuestion*).
- **Family C — anything else we observe** (e.g. select-from-list).

**Sources.** Family A Claude rows are seeded from CodeExam's extraction
(`docs/claude-permissions.md`, build `2026-06-14`). Family B has no
CodeExam data — it is characterized from specimens under
`docs/pty-menu-specimens/`. See that directory's `README.md` for the
capture + routing convention (both the human paste-in channel and the
machine `excerpt=` / `menu_bearing=` trace intake feed it).

**Read the anchoring note first.** CodeExam strings are *source
literals*; the on-screen sentences embed runtime values
(`<file>`/`<host>`/`<key>`), so detection anchors on **fragments**, never
whole sentences. The scanner reads ANSI-stripped PTY bytes — CodeExam's
structured `option.type` / `feedbackConfig.type` fields never reach the
screen and are not matchable.

## Detection axes (columns)

The booleans the `[pty-menu-scan]` trace already emits, plus the two
added for #197:

- **cursor** — `❯` selection anchor before an option (`pty_menu_anchor_pos`).
- **header** — `"Do you want"` present (`rposition` over stripped tail).
- **1./2. pair** — first numbered option + a following `2.`
  within 512 bytes (`needle2_after_anchor` + `anchor_distance_ok`).
- **footer** — a line matching `line_is_menu_footer`
  (`Esc to cancel` · `Tab to amend` · `ctrl+e to explain`).
- **keyword guard** — `pty_text_looks_like_permission_menu` over the
  full stripped tail.
- **menu_bearing** — `pty_skip_buffer_looks_menu_bearing`; gates
  `excerpt=` capture on skips.

## Version pin

| Provider | CLI version (observed 2026-06-25) | CodeExam build |
| --- | --- | --- |
| Claude Code | `2.1.179` | `2026-06-14` (`.cli_js_from_exe_split_NEW`) |
| Codex | `codex-cli 0.142.2` | none yet |

Strings drift between builds — re-pin on a new version.

---

## Family A — tool-permission prompts (Claude)

| Shape | Header fragment | cursor | header | 1./2. pair | footer | Trigger prompt | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| edit | `Do you want to make this edit to` | ✓ | ✓ | ✓ | ✓ | "Add a comment to the top of `<existing file>`" | |
| create | `Do you want to create` | ✓ | ✓ | ✓ | ✓ | "Create `<new file>` containing X" | |
| overwrite | `Do you want to overwrite` | ✓ | ✓ | ✓ | ✓ | "Replace the contents of `<existing file>` with X" | |
| notebook insert | `Do you want to insert this cell into` | ✓ | ✓ | ✓ | ✓ | "Add a cell to `<file>.ipynb`" | |
| notebook delete | `Do you want to delete this cell from` | ✓ | ✓ | ✓ | ✓ | "Remove a cell from `<file>.ipynb`" | |
| proceed (Bash/tool, 3-option) | `Do you want to proceed?` | ✓ | ✓ | ✓ | ✓ | "Run `ls -la`" | The generic tool-use prompt: `1. Yes` / `2. Yes, and don't ask again for similar commands in <dir>` / `3. No`. |
| proceed — manual-approval safety (2-option) | `Do you want to proceed?` | ✓ | ✓ | ✓ | ✓ | various compound / obfuscation patterns: `cd` + output redirection, or quoted / heredoc data (e.g. `cat > f <<'EOF'`) | Only **two** options — `1. Yes` / `2. No`, no "don't ask again" allow-all. The body reason **varies**: "Compound command contains cd with output redirection — manual approval required to prevent path resolution bypass", "Contains data within quote character (expansion obfuscation)", and likely others. All share the `Do you want to proceed?` header, which is why the **header** — not the option count, the box title, or the variable body reason — is the reliable Bash discriminator. See *Signature-less discrimination*. |
| connection | `Do you want to allow this connection?` | ✓ | ✓ | ✓ | ✓ | a tool/command reaching a host outside the sandbox (e.g. `curl https://example.com`) | Hard to evoke deterministically. |
| API key | `Do you want to use this API key?` | ✓ | ✓ | ✓ | ✓ | a tool that wants to use a stored API key | Hard to evoke deterministically. |
| **skill** | `Use skill "<skill>"?` / `from this Skill` | ✓ | **✗** | ✓ | ✓ | "Use the `<skill>` skill to …" | **No `Do you want` header.** Was reaching the keyword guard with no match → `op=skip`. Fixed #197: `use skill` / `from this skill` added to `pty_text_looks_like_permission_menu`. Regression guard — must stay `op=fire`. |

### Exclusions (must NOT fire as tool-permission menus)

CodeExam's "NOT numbered prompts" — onboarding/trust two-button confirms
(`confirmLabel`/`cancelLabel`, not the numbered Select):

```
Yes, I trust these settings   / No, exit Claude Code
Yes, I trust this folder      / No, exit
Yes, I accept                 / No, exit
Yes, allow external imports   / No, disable external imports
Yes, use recommended settings / No, maybe later with /terminal-setup
```

Relevant to the #187 stale-launch buffer (these appear at startup).
Encoding + testing a guard against them is a follow-up — it needs
`menu_bearing` false-positive evidence first.

### Signature-less discrimination (`grid_menu_is_bash_command_box`)

When a permission menu is on screen but its JSONL tool-use signature hasn't
flushed yet, the grid build path must decide whether to build a Bash menu
(`op=build-claude-nosig`) or hold (`op=hold-nosig`) for the signature path.
Defaulting every signature-less frame to Bash mislabeled Edit/Write menus
(Bash/empty/Edit flicker, pane never settled) and invented phantom Bash
menus from prose / tool-call bullets.

`grid_menu_is_bash_command_box(grid_opts, grid_above, grid_header)` builds
Bash on any of three signals, in priority order:

1. **Header `Do you want to proceed?`** — primary. Shared by every Claude
   command-approval shape (3-option, 2-option safety, tall), and distinct
   from Edit (`Do you want to make this edit to <file>?`) and Write (`Do you
   want to create <file>?`). Always captured, so it survives tall command
   boxes.
2. **A `…don't ask again…` option label** — the 3-option Bash allow-all
   (Edit/Write say "allow all edits during this session").
3. **A `Bash command` box title** — fallback, only on frames where it is
   captured.

Edit/Write frames match none of these → `op=hold-nosig` → resolved as
Edit/Write once the signature lands. Phantom frames (prose, tool bullets)
also match none → held → no phantom menu, no spurious NavPanel pulse.

Two discriminators were tried and rejected (the recurrence the header
finally settled):

- **Box title only** — scrolls out of the captured grid window for tall
  command boxes, so real Bash menus were held.
- **Option label only (`don't ask again`)** — absent on the 2-option
  manual-approval safety prompt, so that shape was held.

## Family A — tool-permission prompts (Codex)

Codex prompts go through the `raw_codex_action` path
(`pty_codex_action_required_pos`, "Action Required" title). Not yet
cataloged from a CodeExam run — `docs/codex-permissions.md` is empty.
Follow-up: run CodeExam against `codex` and fill this section.

## Family B — question prompts (Claude Code AskUserQuestion)

A header question + numbered options + a free-text escape — **not** a
tool-permission prompt and not in the CodeExam list. No `Do you want`
header and no permission footer, so the current scanner does not fire on
these. First specimen:
`docs/pty-menu-specimens/2026-06-17-claude-askuserquestion.md`.

| Shape | Header fragment | cursor | header | 1./2. pair | footer | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| askuserquestion | (the question text — variable) | ✓ | ✗ | ✓ | ✗ | Tail options like "Type something" / "Chat about this". Detection + agent-pane surfacing is a follow-up, gated on more specimens. |

## Family C — other

Empty. Add rows as specimens surface unknown shapes.

## Known non-priority edge — self-reference collision

When the agent's *own* on-screen output carries menu-pattern literals —
prose quoting `Do you want to proceed?`, commands grepping `op=fire` /
`❯ 1. Yes 2. No` / `statically analyzed`, etc. — the scanner can anchor
`header`/`cursor` on that **decoy** text while `numbered=false` (no real
options beside the anchor) → `op=skip`, and a genuine menu on screen at
the same time can be missed (never reaches `op=fire`).

Signature to recognize it: a skip with `cursor=true header=true
needle2_after_anchor=true anchor_distance_ok=true` but `numbered=false`,
whose `excerpt=` is agent prose / a command echo rather than a real
numbered block. Evidence: `bram-trace.log` 2026-06-22 ~17:43 (the menu
for a `grep … pty-menu-scan … proceed …` command went 16 skips → no
fire; excerpt captured the prior answer's text + the idle `⏵⏵ accept
edits` prompt).

**Deprioritized — not a fix target.** Confined to sessions where the
agent is itself discussing or grepping menus; ordinary use doesn't print
these literals. Recorded so a future `numbered=false`-with-`header=true`
skip isn't re-investigated from scratch. A real fix would require the
real menu to be a coherent *active* block (header → cursor → numbered
structurally adjacent), not scattered matching tokens — which is exactly
what `numbered=false` already (correctly) refuses.

**Grid-primary update (2026-06-25).** Under the grid detector the analogous
hazard is the grid reading decoy prose / tool-call bullets as a menu and
building a *phantom* Bash menu (which fired the NavPanel pulse with nothing
to show). The signature-less gate now **holds** these frames
(`op=hold-nosig`): decoy prose carries no `Do you want to proceed?` header,
no `don't ask again` option, and no `Bash command` title, so it fails
`grid_menu_is_bash_command_box` and never builds. See *Signature-less
discrimination*.
