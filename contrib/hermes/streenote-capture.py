#!/usr/bin/env python3
"""Capture Hermes prompts, answers, tool calls, and tool results for STREENOTE.

The hook is a passive observer. It writes private, atomic inbox events and never
emits a Hermes hook-protocol directive on stdout.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import sys
import tempfile
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

MAX_EVENT_BYTES = 2 * 1024 * 1024
MAX_TOOL_DETAIL_BYTES = 48 * 1024

SENSITIVE_KEY = re.compile(
    r"(?i)(password|passwd|secret|token|api[_-]?key|authorization|credential|private[_-]?key)"
)
SECRET_ASSIGNMENT = re.compile(
    r"""(?i)\b(api[_-]?key|access[_-]?token|refresh[_-]?token|password|passwd|secret|authorization|credential)\b(\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s,;]+)"""
)
BEARER_VALUE = re.compile(r"(?i)\bBearer\s+[A-Za-z0-9._~+/\-=]+")
SECRET_FLAG = re.compile(
    r"""(?i)(--(?:api[-_]key|token|password|secret)(?:=|\s+))(?:"[^"]*"|'[^']*'|[^\s]+)"""
)


def event_value(payload: dict, extra: dict, key: str):
    value = extra.get(key)
    return payload.get(key) if value is None else value


def make_event_id(event_name: str, session_id: object, identity_id: object) -> str:
    # LLM events use turn_id; tool events use tool_call_id. Including the hook
    # event name keeps the pre and post records for one tool call distinct.
    if identity_id:
        identity = f"{event_name}\0{session_id or ''}\0{identity_id}"
        return hashlib.sha256(identity.encode("utf-8")).hexdigest()
    return uuid.uuid4().hex


def redact_inline(value: str) -> str:
    value = SECRET_ASSIGNMENT.sub(
        lambda match: f"{match.group(1)}{match.group(2)}[REDACTED]", value
    )
    value = BEARER_VALUE.sub("Bearer [REDACTED]", value)
    value = SECRET_FLAG.sub(lambda match: f"{match.group(1)}[REDACTED]", value)
    return value


def redact_value(value):
    if isinstance(value, dict):
        cleaned = {}
        for key, item in value.items():
            if SENSITIVE_KEY.search(str(key)):
                cleaned[key] = "[REDACTED]"
            else:
                cleaned[key] = redact_value(item)
        return cleaned
    if isinstance(value, list):
        return [redact_value(item) for item in value]
    if isinstance(value, tuple):
        return [redact_value(item) for item in value]
    if isinstance(value, str):
        return redact_inline(value)
    return value


def render_detail(value) -> str:
    cleaned = redact_value(value)
    if isinstance(cleaned, str):
        rendered = cleaned
    else:
        rendered = json.dumps(cleaned, ensure_ascii=False, indent=2, sort_keys=True)
    encoded = rendered.encode("utf-8")
    if len(encoded) <= MAX_TOOL_DETAIL_BYTES:
        return rendered
    prefix = encoded[:MAX_TOOL_DETAIL_BYTES].decode("utf-8", errors="ignore")
    return prefix + f"\n...[truncated at {MAX_TOOL_DETAIL_BYTES} UTF-8 bytes]"


def tool_event_text(event_name: str, payload: dict, extra: dict) -> str:
    tool_name = event_value(payload, extra, "tool_name") or "unknown"
    tool_call_id = event_value(payload, extra, "tool_call_id") or "unavailable"

    if event_name == "pre_tool_call":
        args = event_value(payload, extra, "args")
        if args is None:
            args = event_value(payload, extra, "tool_input")
        return (
            f"Tool: {tool_name}\n"
            f"Call ID: {tool_call_id}\n"
            "Arguments:\n"
            f"{render_detail(args) if args is not None else '(not provided)'}"
        )

    status = event_value(payload, extra, "status")
    error_message = event_value(payload, extra, "error_message")
    if not status:
        status = "error" if error_message else "unknown"
    duration_ms = event_value(payload, extra, "duration_ms")
    result = event_value(payload, extra, "result")

    lines = [
        f"Tool: {tool_name}",
        f"Call ID: {tool_call_id}",
        f"Status: {status}",
    ]
    if duration_ms is not None:
        lines.append(f"Duration: {duration_ms} ms")
    lines.append("Result:")
    lines.append(render_detail(result) if result is not None else "(not provided)")

    error_type = event_value(payload, extra, "error_type")
    if error_type:
        lines.append(f"Error type: {render_detail(error_type)}")
    if error_message:
        lines.append(f"Error: {render_detail(error_message)}")
    return "\n".join(lines)


def main() -> int:
    inbox_value = os.environ.get("STREENOTE_HERMES_INBOX")
    if not inbox_value:
        return 0

    try:
        payload = json.load(sys.stdin)
        if not isinstance(payload, dict):
            return 0

        event_name = payload.get("hook_event_name")
        supported_events = (
            "pre_llm_call",
            "post_llm_call",
            "pre_tool_call",
            "post_tool_call",
        )
        if event_name not in supported_events:
            return 0

        extra = payload.get("extra")
        if not isinstance(extra, dict):
            extra = {}

        if event_name == "pre_llm_call":
            kind = "hermes_user"
            text = event_value(payload, extra, "user_message")
        elif event_name == "post_llm_call":
            kind = "hermes_assistant"
            text = event_value(payload, extra, "assistant_response")
        elif event_name == "pre_tool_call":
            kind = "hermes_tool_call"
            text = tool_event_text(event_name, payload, extra)
        else:
            kind = "hermes_tool_result"
            text = tool_event_text(event_name, payload, extra)

        if not isinstance(text, str) or not text.strip():
            return 0

        encoded_text = text.encode("utf-8")
        if len(encoded_text) > MAX_EVENT_BYTES:
            print(
                f"STREENOTE Hermes capture: event exceeds {MAX_EVENT_BYTES} bytes; skipped",
                file=sys.stderr,
            )
            return 0

        session_id = event_value(payload, extra, "session_id")
        if event_name in ("pre_tool_call", "post_tool_call"):
            identity_id = event_value(payload, extra, "tool_call_id")
        else:
            identity_id = event_value(payload, extra, "turn_id")
        event_id = make_event_id(event_name, session_id, identity_id)
        event = {
            "version": 1,
            "event_id": event_id,
            "kind": kind,
            "timestamp": datetime.now(timezone.utc).astimezone().isoformat(timespec="seconds"),
            "text": text,
        }

        inbox = Path(inbox_value).expanduser()
        ready = inbox / "ready"
        ready.mkdir(mode=0o700, parents=True, exist_ok=True)
        try:
            os.chmod(inbox, 0o700)
            os.chmod(ready, 0o700)
        except OSError:
            pass

        final_path = ready / f"{time.time_ns():020d}-{event_id}.json"
        fd, temporary_name = tempfile.mkstemp(prefix=".streenote-", suffix=".tmp", dir=ready)
        try:
            with os.fdopen(fd, "wb") as temporary:
                os.fchmod(temporary.fileno(), 0o600)
                data = json.dumps(event, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
                temporary.write(data)
                temporary.write(b"\n")
                temporary.flush()
                os.fsync(temporary.fileno())
            os.replace(temporary_name, final_path)
        except Exception:
            try:
                os.unlink(temporary_name)
            except OSError:
                pass
            raise

        return 0
    except Exception as exc:
        print(f"STREENOTE Hermes capture failed: {exc}", file=sys.stderr)
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
