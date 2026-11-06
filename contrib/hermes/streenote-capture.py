#!/usr/bin/env python3
"""Capture Hermes CLI prompts and final answers for STREENOTE's journal.

The hook is intentionally a no-op outside a STREENOTE-launched shell because
STREENOTE_HERMES_INBOX is exported only by STREENOTE's PTY shell.
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
import tempfile
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

MAX_EVENT_BYTES = 2 * 1024 * 1024


def event_value(payload: dict, extra: dict, key: str):
    value = extra.get(key)
    return payload.get(key) if value is None else value


def make_event_id(event_name: str, session_id: object, turn_id: object) -> str:
    # A turn ID makes retries idempotent. Older Hermes versions that omit it
    # get a unique ID rather than risking collisions between repeated prompts.
    if turn_id:
        identity = f"{event_name}\0{session_id or ''}\0{turn_id}"
        return hashlib.sha256(identity.encode("utf-8")).hexdigest()
    return uuid.uuid4().hex


def main() -> int:
    inbox_value = os.environ.get("STREENOTE_HERMES_INBOX")
    if not inbox_value:
        return 0

    try:
        payload = json.load(sys.stdin)
        if not isinstance(payload, dict):
            return 0

        event_name = payload.get("hook_event_name")
        if event_name not in ("pre_llm_call", "post_llm_call"):
            return 0

        extra = payload.get("extra")
        if not isinstance(extra, dict):
            extra = {}

        if event_name == "pre_llm_call":
            kind = "hermes_user"
            text = event_value(payload, extra, "user_message")
        else:
            kind = "hermes_assistant"
            text = event_value(payload, extra, "assistant_response")

        if not isinstance(text, str) or not text.strip():
            return 0

        encoded_text = text.encode("utf-8")
        if len(encoded_text) > MAX_EVENT_BYTES:
            print(
                f"STREENOTE Hermes capture: message exceeds {MAX_EVENT_BYTES} bytes; skipped",
                file=sys.stderr,
            )
            return 0

        session_id = event_value(payload, extra, "session_id")
        turn_id = event_value(payload, extra, "turn_id")
        event_id = make_event_id(event_name, session_id, turn_id)
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

        # A silent hook is important: stdout is reserved for Hermes hook
        # protocol responses and must not contain journal text.
        return 0
    except Exception as exc:
        print(f"STREENOTE Hermes capture failed: {exc}", file=sys.stderr)
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
