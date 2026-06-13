#!/usr/bin/env python3
"""Normalize a bram-trace.log slice for snapshot comparison.

Reads trace lines from stdin (one per line), strips per-run noise
(timestamps, correlation IDs, host_ms, pids, nonces, deltas), preserves
the structural fields that name the event's behavior (kind, subkind,
op, state, reason, tool, source, decision, target, path, phase, etc.),
and writes one normalized line per input line to stdout.

The output is deterministic for any given input: re-running against
the same input yields byte-identical output. Two recordings of the
same UI gesture against unchanged code should diff cleanly via a
plain `diff -u`.

Trace line shapes recognized:
  [ISO8601Z] [kind] k1=v1 k2=v2 ...
  [ISO8601Z] [kind] subkind=NAME {json-object}
  [ISO8601Z] [kind] free text with k=v segments
"""
from __future__ import annotations

import json
import re
import sys
from typing import Any


# Keys whose values are timing — strip in structural mode (the default),
# preserve in --keep-timing mode. Kept distinct from non-timing noise
# so the two modes can compose: --keep-timing is "structural normalizer
# but keep enough timing fields to compute delta_to_emit_ms
# distributions for lag investigation across N runs of the same
# gesture."
_TIMING_KEYS = {
    "at",
    "at_host_ms",
    "delta_to_emit_ms",
    "duration_ms",
    "elapsed_ms",
    "elapsedMs",
    "elapsed_ms_total",
    "issuedAtMs",
    "issued_at_ms",
    "last_host_ms",
    "started_ms",
    "startedAtMs",
}

# Non-timing per-run noise. Stripped in both modes. Identifiers that
# carry no semantic meaning (correlation IDs, nonces, pids, session
# identifiers, file mtimes captured in trace fields).
_NON_TIMING_NOISE_KEYS = {
    "correlation_id",
    "mtime",
    "nonce",
    "pid",
    "session",
    "sid",
    "ts",
}

# Default noise set (structural mode): everything from both above.
_NOISE_KEYS = _TIMING_KEYS | _NON_TIMING_NOISE_KEYS

# Substrings that, when seen in a value, get replaced with a placeholder
# so paths like /tmp/foo-<pid>-<rand>/ or /Users/jonudell/.cache/bram/
# paste/<unix-ms>-<hash>.png do not introduce per-run drift.
_PATH_NORMALIZE_RX = [
    (re.compile(r"\.tmp\.\d+\.[0-9a-f]+"), ".tmp.<PID>.<RAND>"),
    (re.compile(r"/tmp/[a-zA-Z0-9_.-]+/"), "/tmp/<TMP>/"),
    (
        re.compile(r"/[^/\s]+/\.cache/bram/paste/\d+-[0-9a-f]+"),
        "/<HOME>/.cache/bram/paste/<UNIX_MS>-<HASH>",
    ),
    (re.compile(r"turn=\d{13}"), "turn=<UNIX_MS>"),
    (re.compile(r"-\d{13}-\d+"), "-<UNIX_MS>-<N>"),
    (re.compile(r"-\d{13}"), "-<UNIX_MS>"),
]


_HEADER_RX = re.compile(r"^\[(?P<ts>[^\]]+)\]\s+\[(?P<kind>[^\]]+)\]\s*(?P<body>.*)$")
_SUBKIND_JSON_RX = re.compile(r"^subkind=(?P<sub>\S+)\s+(?P<json>\{.*\})\s*$")
_KV_RX = re.compile(r"(?P<key>[A-Za-z_][A-Za-z0-9_]*)=(?P<val>\S+)")


# Noise filter. The trace records continuous background activity
# (heartbeats, polling routes, scanner skips, raw terminal data) that
# has nothing to do with a user-driven gesture. Dropping it makes the
# snapshot small enough to actually diff. A single Drop gesture went
# from 1559 lines (mostly noise) to a couple dozen after this filter
# landed.
#
# Anything not in these sets is preserved. When in doubt, keep — a
# false positive (real signal dropped) silently corrupts a snapshot;
# a false negative (noise preserved) just makes the diff a bit bigger.
_NOISE_KINDS = {
    "[status-substate]",   # agent status phrase tracking, not behavioral
    "[pty-in]",            # raw terminal data chunks, too low level
}
_NOISE_IFRAME_SUBKINDS = {
    "heartbeat-batch",
    "heartbeat-drift",
    "heartbeat-tick",
    "inspector-tap-tick",
    "agent-header-status-loaded",
    "agent-header-branch",
    "worklist-ui-state-save",
    "talk-session-batch",
    "jsonl-pipeline-ms",
    "jsonl-fanout",
    "jsonl-broadcast",
}
_POLLING_GET_ROUTES = {
    "__last-exchange",
    "__turn-state",
    "__agent-status",
    "__inflight",
    "__right-pane-info",
    "__enhance/status",
    "__app-info",
    "__commits",
    "__sessions/latest-tail",
    "__current-turn-edits",
    "__last-assistant-text",
    "__worklist",
}
_NOISY_PTY_MENU_SCAN_PREFIX = "[pty-menu-scan] op=skip"
_NOISY_WATCHER_PREFIX = "[watcher] dispatch=skip"


_LEADING_TS_RX = re.compile(r"^\[[^\]]+\]\s+(?=\[)")


def _is_noise_normalized(line: str) -> bool:
    """True iff the normalized line is part of background noise that
    no user-driven gesture should care about. Handles both modes:
    structural lines start with [kind], --keep-timing lines start
    with [ISO8601Z] [kind]."""
    body = _LEADING_TS_RX.sub("", line, count=1)
    for kind in _NOISE_KINDS:
        if body.startswith(kind):
            return True
    if body.startswith("[iframe] subkind="):
        for sub in _NOISE_IFRAME_SUBKINDS:
            if body.startswith(f"[iframe] subkind={sub} ") or body == f"[iframe] subkind={sub}":
                return True
    if body.startswith(_NOISY_PTY_MENU_SCAN_PREFIX):
        return True
    if body.startswith(_NOISY_WATCHER_PREFIX):
        return True
    if body.startswith("[route] "):
        for path in _POLLING_GET_ROUTES:
            if f"method=GET path={path}" in body:
                return True
    return False


def _scrub_value(s: str) -> str:
    for rx, repl in _PATH_NORMALIZE_RX:
        s = rx.sub(repl, s)
    return s


def _scrub_json(obj: Any, noise_keys: set[str]) -> Any:
    if isinstance(obj, dict):
        out = {}
        for k, v in obj.items():
            if k in noise_keys:
                continue
            out[k] = _scrub_json(v, noise_keys)
        return out
    if isinstance(obj, list):
        return [_scrub_json(v, noise_keys) for v in obj]
    if isinstance(obj, str):
        return _scrub_value(obj)
    return obj


def _normalize_kv_body(body: str, noise_keys: set[str]) -> str:
    """Body is `k1=v1 k2=v2 ...`. Drop noise keys, scrub remaining values."""
    parts = []
    for m in _KV_RX.finditer(body):
        key = m.group("key")
        val = m.group("val")
        if key in noise_keys:
            continue
        parts.append(f"{key}={_scrub_value(val)}")
    return " ".join(parts)


def _normalize_line(line: str, noise_keys: set[str], keep_timestamp: bool) -> str | None:
    """Return a normalized representation of one trace line, or None to drop."""
    line = line.rstrip("\n")
    if not line:
        return None
    m = _HEADER_RX.match(line)
    if not m:
        return _scrub_value(line)
    ts = m.group("ts")
    kind = m.group("kind")
    body = m.group("body")
    prefix = f"[{ts}] " if keep_timestamp else ""
    sub = _SUBKIND_JSON_RX.match(body)
    if sub:
        try:
            payload = json.loads(sub.group("json"))
        except json.JSONDecodeError:
            return f"{prefix}[{kind}] subkind={sub.group('sub')} {_scrub_value(sub.group('json'))}"
        scrubbed = _scrub_json(payload, noise_keys)
        payload_str = json.dumps(scrubbed, sort_keys=True, separators=(",", ":"))
        return f"{prefix}[{kind}] subkind={sub.group('sub')} {payload_str}"
    return f"{prefix}[{kind}] {_normalize_kv_body(body, noise_keys)}".rstrip()


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser(
        description=(
            "Normalize a bram-trace.log slice for snapshot comparison. "
            "Default mode strips all per-run noise (timestamps, correlation "
            "IDs, host_ms deltas, pids, nonces) for deterministic structural "
            "equivalence checks. --keep-timing preserves the leading "
            "timestamp and timing-related fields (at_host_ms, "
            "delta_to_emit_ms, elapsed_ms, etc.) so the same gesture's "
            "snapshot can be used for lag / perf distributions across N "
            "runs instead of just for equivalence."
        )
    )
    parser.add_argument(
        "--keep-timing",
        action="store_true",
        help=(
            "Preserve timing fields and the leading ISO8601 timestamp. "
            "Output is no longer deterministic across runs of the same "
            "gesture; that's the point — you can subtract timestamps to "
            "compute per-event latency."
        ),
    )
    args = parser.parse_args()

    if args.keep_timing:
        noise_keys = _NON_TIMING_NOISE_KEYS
    else:
        noise_keys = _NOISE_KEYS

    for raw in sys.stdin:
        norm = _normalize_line(raw, noise_keys, keep_timestamp=args.keep_timing)
        if norm is None:
            continue
        if _is_noise_normalized(norm):
            continue
        print(norm)
    return 0


if __name__ == "__main__":
    sys.exit(main())
