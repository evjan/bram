# PTY menu specimens

Raw observed instances of interactive menus emitted by the agent CLIs
(Claude Code, Codex). This is the **specimen corpus** — many specimens
may map to one curated row in `../pty-menu-shapes.md`, and `unknown`
specimens are allowed to sit here before a catalog row exists.

Curated model vs. raw corpus:

- `../pty-menu-shapes.md` — the deduplicated catalog (one row per shape).
- this directory — every raw observation, as captured.

## Two intakes

Both terminate here, then feed curated rows in `../pty-menu-shapes.md`:

1. **Machine** — the `[pty-menu-scan]` trace in
   `resources/bram-traces/bram-trace.log`. On `op=fire` it carries a
   stripped `excerpt='…'`; on `op=skip` it carries `menu_bearing=` and,
   when that is `true`, an `excerpt='…'` (the "looks cataloged but the
   scanner skipped" high-signal case). Workflow: grep
   `op=fire .* excerpt=` and `op=skip menu_bearing=true`, compare to the
   catalog, confirm a row or file a new specimen.
2. **Human paste-in** — paste a screenshot or text of a menu you see;
   the agent routes it here (see *Routing* below).

## File layout

- One file per specimen: `<date>-<provider>-<shape>.md`
  (`shape` = a catalog id, or `unknown`). Example:
  `2026-06-17-claude-askuserquestion.md`.
- Screenshots are **copied into this directory** as `<same-stem>.png`.
  The paste cache (`~/.cache/bram/paste/`) is ephemeral, so a specimen
  that only referenced a cache path would rot.

### Frontmatter

```yaml
---
observed: 2026-06-17        # date seen
provider: claude            # claude | codex
cli_version: 2.1.169        # `claude --version` / `codex --version`
shape: askuserquestion      # catalog id, or `unknown`
source: screenshot          # screenshot | paste | trace-excerpt
screenshot: 2026-06-17-claude-askuserquestion.png   # optional
---
```

Body: the stripped menu text, then notes — which detection axes matched
(`cursor` / `header` / `1./2.` pair / `footer`), and how it differs from
sibling shapes.

## Routing (what the agent does on a paste)

1. Extract the stripped text — read it from the screenshot, or take the
   pasted text verbatim.
2. Copy the screenshot into this directory next to the specimen.
3. Decide **is this something we already have?** Compare against
   `../pty-menu-shapes.md` rows and existing specimens. Confirm a match,
   or mark `unknown` / add a new catalog row when it doesn't fit.
4. Assign a useful filename (`<date>-<provider>-<shape>.md`) and write
   the specimen.

Specimen filing is **append-only docs** — individual paste-ins are
direct edits, not a fresh worklist propose→approve cycle.
