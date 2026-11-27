"""Validate the AMC M1 policy-event JSONL contract defined below.

Closed contract: only known fields, typed values, fixed vocabularies.
Rejections never echo input values.

Usage: python3 scripts/amc-headroom-policy-events-check.py [events.jsonl]
With no argument, runs the built-in assert self-check.
Exit 0 = contract holds; nonzero = first violation printed.
"""

import json
import sys
import uuid

REQUIRED = {"event", "policyRev", "host", "unit", "invocationId",
            "entryPoint", "workloadClass", "action", "reason",
            "startedMs", "outcome"}
OPTIONAL = {"endedMs", "limitBytes", "observedBytes", "details"}
ALLOWED = REQUIRED | OPTIONAL
STR_FIELDS = (ALLOWED - {"startedMs", "endedMs", "limitBytes",
                         "observedBytes", "details"})
INT_FIELDS = {"startedMs", "endedMs", "limitBytes", "observedBytes"}
EVENTS = {"policy.applied", "memory.observed", "admission.blocked",
          "admission.resumed", "throttle.entered", "throttle.cleared",
          "oom.attributed", "quiescence.entered", "quiescence.exited",
          "drain.started", "drain.completed", "recovery.verified",
          "recovery.failed"}
ENTRY_POINTS = {"gamemode-hook", "operator-cli", "scheduled", "worker"}
WORKLOAD_CLASSES = {"service-floor", "games", "interactive-dev",
                    "background-workers", "builds-transfers-maintenance"}
OUTCOMES = {"ok", "deferred", "failed", "unknown"}
REASONS = {"gamemode-start", "gamemode-end", "operator-request",
           "memory-pressure", "pressure-cleared", "oom-kill",
           "config-applied", "health-check", "timeout", "dependency-down",
           "maintenance-window", "unknown"}
FORBIDDEN = {"token", "password", "secret", "authorization", "cookie",
             "prompt", "command", "argv", "environment", "document",
             "content", "body", "email", "userid"}
DETAILS_ALLOWED = {"slice", "cgroup", "threshold", "window", "policy"}
MAX_DETAIL = 256
MAX_DETAILS = 32
MAX_LINE = 65536


def _pairs(pairs: list) -> dict:
    seen = set()
    for k, _ in pairs:
        if k in seen:
            raise ValueError(f"duplicate key {k!r}")
        seen.add(k)
    return dict(pairs)


def check_line(line: str, lineno: int) -> str | None:
    if len(line) > MAX_LINE:
        return f"line {lineno}: record too large (size withheld)"
    try:
        obj = json.loads(line, object_pairs_hook=_pairs)
    except (json.JSONDecodeError, ValueError):
        return f"line {lineno}: invalid JSON"
    if not isinstance(obj, dict):
        return f"line {lineno}: not an object"
    unknown = set(obj) - ALLOWED
    if unknown:
        return f"line {lineno}: unknown fields (rejected, values withheld)"
    missing = REQUIRED - set(obj)
    if missing:
        return f"line {lineno}: missing {sorted(missing)}"
    for f in INT_FIELDS & set(obj):
        if not isinstance(obj[f], int) or isinstance(obj[f], bool):
            return f"line {lineno}: {f} must be an integer"
        if obj[f] < 0:
            return f"line {lineno}: {f} must be non-negative"
    if ("startedMs" in obj and "endedMs" in obj
            and obj["endedMs"] < obj["startedMs"]):
        return f"line {lineno}: endedMs precedes startedMs"
    for f in STR_FIELDS & set(obj):
        if not isinstance(obj[f], str):
            return f"line {lineno}: {f} must be a string"
    if obj["event"] not in EVENTS:
        return f"line {lineno}: unknown event (value withheld)"
    if obj["entryPoint"] not in ENTRY_POINTS:
        return f"line {lineno}: unknown entryPoint (value withheld)"
    if obj["workloadClass"] not in WORKLOAD_CLASSES:
        return f"line {lineno}: unknown workloadClass (value withheld)"
    if obj["outcome"] not in OUTCOMES:
        return f"line {lineno}: unknown outcome (value withheld)"
    if obj["reason"] not in REASONS:
        return f"line {lineno}: unknown reason (value withheld)"
    try:
        uuid.UUID(str(obj["invocationId"]))
    except ValueError:
        return f"line {lineno}: invocationId must be a UUID"
    for f in ("policyRev", "host", "unit", "invocationId", "action"):
        if not obj[f]:
            return f"line {lineno}: {f} must be non-empty"
    lowered = {str(k).lower() for k in obj}
    if FORBIDDEN & lowered:
        return f"line {lineno}: forbidden field present (name withheld)"
    details = obj.get("details", {})
    if not isinstance(details, dict):
        return f"line {lineno}: details must be a flat map"
    if len(details) > MAX_DETAILS:
        return f"line {lineno}: too many detail keys"
    unknown_detail = set(details) - DETAILS_ALLOWED
    if unknown_detail:
        return f"line {lineno}: unknown detail keys (rejected, values withheld)"
    if FORBIDDEN & {k.lower() for k in details}:
        return f"line {lineno}: forbidden detail key present (name withheld)"
    for v in details.values():
        if not isinstance(v, str) or len(v) > MAX_DETAIL:
            return f"line {lineno}: bad detail value (key withheld)"
    return None


def self_check() -> None:
    base = ('{"event": "quiescence.entered", "policyRev": "r1", '
            '"host": "example-host", "unit": "background-quiesce.service", '
            '"invocationId": "12345678-1234-5678-1234-567812345678", '
            '"entryPoint": "gamemode-hook", "workloadClass": "games", '
            '"action": "freeze", "reason": "gamemode-start", '
            '"startedMs": 1, "outcome": "ok", '
            '"details": {"slice": "background.slice"}}')
    assert check_line(base, 1) is None
    cases = [
        base.replace('"quiescence.entered"', '"naptime"'),
        base.replace('"slice"', '"password"'),
        base.replace('"invocationId": "12345678-1234-5678-1234-567812345678"',
                     '"invocationId": "a"'),
        base.replace('"gamemode-hook"', '"cron"'),
        base.replace('"games"', '"everything"'),
        base.replace('"ok"', '"fine"'),
        base.replace('"gamemode-start"', '"because"'),
        base.replace('"startedMs": 1', '"startedMs": "now"'),
        base.replace('"outcome": "ok"', '"outcome": "ok", "extra": 1'),
        base.replace('"unit": "background-quiesce.service"',
                     '"unit": "background-quiesce.service", "Token": "x"'),
        base.replace('"reason": "gamemode-start"',
                     '"reason": "gamemode-start", "reason": "dup"'),
        '{"event": "policy.applied"}',
        ('{"event": 7, "policyRev": "r", "host": "h", "unit": "u", '
         '"invocationId": "12345678-1234-5678-1234-567812345678", '
         '"entryPoint": "scheduled", "workloadClass": "games", '
         '"action": "a", "reason": "timeout", "startedMs": 1, "outcome": "ok"}'),
        base.replace('"startedMs": 1', '"startedMs": -1'),
        base.replace('"outcome": "ok"', '"outcome": "ok", "endedMs": 0'),
        base.replace('"details": {"slice": "background.slice"}',
                     '"details": {"access_token": "x"}'),
        base.replace('"unit": "background-quiesce.service"',
                     '"unit": ""'),
        'not json',
    ]
    for i, bad in enumerate(cases, 2):
        assert check_line(bad, i) is not None, f"case {i} accepted"
    print(f"self-check passed: good accepted, bad rejected ({len(cases)} cases)")


if __name__ == "__main__":
    if len(sys.argv) == 1:
        self_check()
    else:
        with open(sys.argv[1]) as f:
            for i, line in enumerate(f, 1):
                if line.strip() and (err := check_line(line, i)):
                    print(err)
                    sys.exit(1)
        print(f"contract holds: {sys.argv[1]}")
