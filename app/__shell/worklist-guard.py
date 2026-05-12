#!/usr/bin/env python3
"""PreToolUse hook: enforce the two-stage worklist flow for resources/worklist.json.

Block Write/Edit operations that remove an item with status "proposed" (or
unset) unless the user's most recent transcript message authorized that
removal via `drop: {"ids":[...]}`. Items already at status "applied" may be
removed freely (commit-then-prune is legitimate).
"""

import json
import os
import sys


def items_by_id(text):
    try:
        return {it["id"]: it for it in json.loads(text).get("items", [])}
    except Exception:
        return {}


def last_user_text(transcript_path):
    if not transcript_path or not os.path.exists(transcript_path):
        return ""
    last = ""
    with open(transcript_path) as f:
        for line in f:
            try:
                m = json.loads(line)
            except Exception:
                continue
            if m.get("type") != "user":
                continue
            c = m.get("message", {}).get("content", "")
            if isinstance(c, list):
                c = "".join(
                    p.get("text", "") for p in c
                    if isinstance(p, dict) and p.get("type") == "text"
                )
            if isinstance(c, str):
                last = c
    return last


def parse_auth(msg):
    """Return (kind, ids) for `approved:` or `drop:` prefixed messages."""
    msg = msg.strip()
    for prefix, kind in (("approved:", "approved"), ("drop:", "drop")):
        if msg.startswith(prefix):
            try:
                data = json.loads(msg[len(prefix):].strip())
            except Exception:
                return kind, set()
            if kind == "drop":
                return kind, set(data.get("ids", []))
            return kind, {
                it.get("id") for it in data.get("items", [])
                if isinstance(it, dict)
            }
    return None, set()


def main():
    payload = json.load(sys.stdin)
    if payload.get("tool_name") not in ("Write", "Edit"):
        sys.exit(0)

    ti = payload.get("tool_input", {})
    fp = ti.get("file_path", "")
    if not fp.endswith("/resources/worklist.json") or not os.path.exists(fp):
        sys.exit(0)

    with open(fp) as f:
        old = f.read()

    if payload["tool_name"] == "Write":
        new = ti.get("content", "")
    else:
        o = ti.get("old_string", "")
        n = ti.get("new_string", "")
        new = old.replace(o, n) if ti.get("replace_all") else old.replace(o, n, 1)

    old_items = items_by_id(old)
    new_items = items_by_id(new)
    removed = set(old_items) - set(new_items)
    if not removed:
        sys.exit(0)

    kind, ids = parse_auth(last_user_text(payload.get("transcript_path", "")))

    violations = []
    for rid in removed:
        st = old_items[rid].get("status", "proposed")
        if st == "applied":
            continue
        if kind == "drop" and rid in ids:
            continue
        violations.append((rid, st))

    if violations:
        bad = ", ".join(f'"{r}" (status={s})' for r, s in violations)
        print(
            f"Blocked: removing {bad} from resources/worklist.json without "
            f"going through the two-stage flow.\n"
            f"  - 'proposed' must transition to 'applied' (re-add the item) "
            f"before pruning.\n"
            f"  - Direct removal is allowed only when the user's last "
            f"message was `drop: {{\"ids\":[...]}}`.\n"
            f"Last user authorization kind: {kind or 'none'}.",
            file=sys.stderr,
        )
        sys.exit(2)

    sys.exit(0)


if __name__ == "__main__":
    main()
