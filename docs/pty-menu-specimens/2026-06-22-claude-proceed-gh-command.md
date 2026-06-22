---
observed: 2026-06-22
provider: claude
cli_version: 2.1.170
shape: proceed
source: screenshot
screenshot: 2026-06-22-claude-proceed-gh-command.png
---

# proceed (gh command) — command-family scoping, fires

The permission prompt for `gh issue comment …`, captured after removing
`Bash(gh issue *)` and **restarting** (permission-rule changes only take
effect on restart), and run **without** a `cd` prefix so the prompt scopes
to the gh command itself. It fired (`op=fire`).

## Captured (stripped)

```
Bash command
  gh issue comment 205 --repo judell/bram --body "Clean retry of the permission test without the cd prefix …"
This command requires approval
Do you want to proceed?
❯ 1. Yes
  2. Yes, and don't ask again for: gh issue *
  3. No
Esc to cancel · Tab to amend · ctrl+e to explain
```

## Why this matters — the option-2 scoping taxonomy

The miss is decided by how Claude scopes the "don't ask again" option, and
there are (so far) three forms:

1. **Command-family glob** — `don't ask again for: gh issue *` (this
   specimen). Short, stable, **fires**. git/gh recognized commands get
   this.
2. **Tool-in-dir** — `don't ask again for <server — tool> commands in
   <dir>` (MCP, `2026-06-22-claude-proceed-mcp-tool`). Also short, **fires**.
3. **Full command** — `don't ask again for <entire command>`
   (`2026-06-22-claude-proceed-long-command`, the playwright case). Only
   this one grows long enough to wrap and interleave the `2.` token →
   **never fires**.

So the **proceed-long miss is specific to un-scopable commands** (full
command embedded), **not** to git/gh — which always get the short
command-family glob (form 1). This closes the git/gh hunt: read-only git
(`git status`) is auto-approved and never prompts; write git/gh prompt but
scope short and fire. Neither reproduces the long-miss.

## Notes

- Also a clean confirmation of the `cd`-prefix confound: with the prefix,
  option 2 was `cd *` (scope hijacked to the leading `cd`); without it,
  `gh issue *` (correct). See feedback on not cd-prefixing Bash commands.
- The command was denied at the prompt (captured, not approved), so no
  comment was posted to #205.
