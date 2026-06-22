# PTY menu shapes — detection-axis catalog

**What this is:** the shared model of *which interactive menus Bram's PTY
scanner should surface*, and for each, which detection axes in
`src-tauri/src/lib.rs` (`pty_menu_detect`, `pty_menu_scan_diagnostic`)
catch it. It turns reactive tuning into a checklist: every cataloged
shape must produce `op=fire`; a miss becomes "shape X regressed on axis
Y" instead of a guess. Refs #197.

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

| Provider | CLI version (observed 2026-06-17) | CodeExam build |
| --- | --- | --- |
| Claude Code | `2.1.169` | `2026-06-14` (`.cli_js_from_exe_split_NEW`) |
| Codex | `codex-cli 0.140.0` | none yet |

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
| proceed (Bash/tool) | `Do you want to proceed?` | ✓ | ✓ | ✓ | ✓ | "Run `ls -la`" | The generic tool-use prompt. |
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
