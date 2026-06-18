# Claude Code permission prompts — quick string list

**What this is:** the human-visible strings you'll see in Claude Code's
numbered-choice / permission prompts. A fast orientation, like CodeExam's
"LLM Prompts" list.

**Read this first (so the list isn't misleading):**
- `<angle-bracket>` = a value filled in at runtime (file, host, command, dir,
  keybinding). These are **not** part of the literal — e.g. the on-screen
  "Yes, and always allow access to `tmp` from this project" exists in source only
  as the fragments `"Yes, and always allow access to "` + `<dir>` + `" from this project"`.
- So **for detection, don't grep these whole sentences** — use
  `CC_permission_prompt_detector_spec.md` (anchor on fragments + the
  `option.type` / `feedbackConfig.type` fields). This list is "what the user
  sees," not "what to match."
- Built from `.cli_js_from_exe_split_NEW` (2026-06-14). Strings drift between
  builds — re-pin on a new version. Full detail + evidence:
  `CC_permission_prompt_detection_results_2.md`.

---

## Questions / headlines

```
Do you want to proceed?
Do you want to make this edit to <file>?
Do you want to create <file>?
Do you want to overwrite <file>?
Do you want to insert this cell into <file>?
Do you want to delete this cell from <file>?
Do you want to make this edit to <file>?            (notebook)
Do you want to allow this connection?
Do you want to use this API key?
Use skill "<skill>"?
```

Titles / banners that accompany them:
```
Tool use
Edit file   ·   Create file   ·   Overwrite file   ·   Edit notebook   ·   Read file
Network request outside of sandbox        Host: <host>
Claude may use instructions, code, or files from this Skill.
```

## The numbered choices (the 1 / 2 / 3 options)

Always present:
```
Yes
No
```

Accept-and-remember variants (which one appears depends on the tool & scope):
```
Yes, during this session
Yes, allow all edits during this session (<key>)
Yes, allow reading from <dir>/ during this session
Yes, allow all edits in <dir>/ during this session (<key>)
Yes, and allow Claude to edit its own settings for this session
Yes, and always allow access to <dir> from this project
Yes, allow reading from <path> from this project
Yes, and allow access to <path> and <cmd> commands
Yes, and allow <paths> access and <cmd> commands
Yes, and don't ask again for <host>                          (network)
Yes, and don't ask again for <tool> commands in <cwd>        (tool / bash)
Yes, and don't ask again for <skill> in <cwd>                (skill, exact)
Yes, and don't ask again for <prefix>:* commands in <cwd>    (skill, prefix)
Yes, for this session                                        (directory trust)
Yes, and remember this directory                             (directory trust)
```

"No" variants:
```
No, and tell Claude what to do differently (esc)
No, not now
No, and don't show plugin installation hints again
Don't ask again
```

Feedback-mode input placeholders (shown when you expand Yes/No to add a note):
```
and tell Claude what to do next          (on Yes)
and tell Claude what to do differently   (on No)
```

⚠ `(<key>)` above is a **templated keybinding** — usually `(shift+tab)`, but can
differ on Windows / older Node. Don't treat `(shift+tab)` as fixed text.

---

## NOT numbered prompts (two-button confirm — listed so you don't conflate them)

These use a separate Yes/No confirm widget (`confirmLabel`/`cancelLabel`), not
the numbered Select. They're onboarding/trust dialogs, not tool-permission prompts:
```
Yes, I trust these settings        /  No, exit Claude Code
Yes, I trust this folder           /  No, exit
Yes, I accept                      /  No, exit
Yes, allow external imports        /  No, disable external imports
Yes, use recommended settings      /  No, maybe later with /terminal-setup
```

## Related UI (not a prompt, but you'll see it near them)
```
Press shift+tab to cycle permission modes ...
shift+tab, plan, auto                          (mode tagline)
```
Permission modes: `default` · `acceptEdits` · `bypassPermissions` · `plan` · `dontAsk` · `auto`.