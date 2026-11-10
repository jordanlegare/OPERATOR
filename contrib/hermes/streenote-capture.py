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
import stat
import sys
import tempfile
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

MAX_EVENT_BYTES = 8 * 1024 * 1024
MAX_TOOL_DETAIL_BYTES = 48 * 1024
MAX_HISTORY_BYTES = 1024 * 1024

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


def _review_instance_is_active(registry: Path, instance_id: str) -> bool:
    """Return True only when the named STREENOTE listener is still registered and live."""
    if not isinstance(instance_id, str) or not re.fullmatch(r"[a-f0-9-]{1,64}", instance_id):
        return False
    manifest = registry / f"{instance_id}.json"
    socket_path = registry / f"{instance_id}.sock"
    try:
        manifest_stat = manifest.lstat()
        if (
            not stat.S_ISREG(manifest_stat.st_mode)
            or manifest_stat.st_uid != os.getuid()
            or stat.S_IMODE(manifest_stat.st_mode) & 0o077
            or manifest_stat.st_size > 4096
        ):
            return False
        registration = json.loads(manifest.read_text(encoding="utf-8"))
        if not isinstance(registration, dict) or registration.get("version") != 1:
            return False
        if registration.get("instance_id") != instance_id:
            return False
        registered_socket = registration.get("socket_path")
        if registered_socket and Path(registered_socket) != socket_path:
            return False
        socket_stat = socket_path.lstat()
        return (
            stat.S_ISSOCK(socket_stat.st_mode)
            and socket_stat.st_uid == os.getuid()
            and not (stat.S_IMODE(socket_stat.st_mode) & 0o077)
        )
    except (OSError, ValueError, TypeError, json.JSONDecodeError):
        return False




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




def render_history_value(value) -> str:
    cleaned = redact_value(value)
    if isinstance(cleaned, str):
        return cleaned
    return json.dumps(cleaned, ensure_ascii=False, indent=2, sort_keys=True)


def history_snapshot_text(history) -> str | None:
    """Render prior user/assistant/tool messages without system or developer prompts."""
    if isinstance(history, dict):
        history = history.get("messages")
    if not isinstance(history, list):
        return None

    rendered_messages = []
    for message in history:
        if not isinstance(message, dict):
            continue
        role = str(message.get("role") or message.get("type") or "").strip().lower()
        role = {
            "human": "user",
            "ai": "assistant",
            "function": "tool",
        }.get(role, role)
        if role not in {"user", "assistant", "tool"}:
            continue

        name = message.get("name") or message.get("tool_name")
        header = role.upper()
        if isinstance(name, str) and name.strip():
            header += f" ({redact_inline(name.strip())})"

        content = message.get("content")
        if content is None:
            content = message.get("text")
        parts = []
        if content is not None and content != "":
            parts.append(render_history_value(content))
        tool_calls = message.get("tool_calls")
        if tool_calls:
            parts.append("Tool calls:\n" + render_history_value(tool_calls))
        if not parts:
            continue
        rendered_messages.append(header + "\n" + "\n".join(parts))

    if not rendered_messages:
        return None

    heading = (
        "PRIOR CONVERSATION CONTEXT "
        f"({len(rendered_messages)} message(s); system/developer messages excluded)"
    )
    snapshot = heading + "\n\n" + "\n\n".join(
        f"[{index}] {message}"
        for index, message in enumerate(rendered_messages, start=1)
    )
    encoded = snapshot.encode("utf-8")
    if len(encoded) > MAX_HISTORY_BYTES:
        prefix = encoded[:MAX_HISTORY_BYTES].decode("utf-8", errors="ignore")
        snapshot = prefix + (
            f"\n\n...[history snapshot truncated at {MAX_HISTORY_BYTES} UTF-8 bytes]"
        )
    return snapshot


def _write_inbox_event_one(inbox: Path, event: dict) -> None:
    ready = inbox / "ready"
    ready.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        os.chmod(inbox, 0o700)
        os.chmod(ready, 0o700)
    except OSError:
        pass

    event_id = str(event["event_id"])
    final_path = ready / f"{time.time_ns():020d}-{event_id}.json"
    fd, temporary_name = tempfile.mkstemp(
        prefix=".streenote-", suffix=".tmp", dir=ready
    )
    try:
        with os.fdopen(fd, "wb") as temporary:
            os.fchmod(temporary.fileno(), 0o600)
            data = json.dumps(
                event, ensure_ascii=False, separators=(",", ":")
            ).encode("utf-8")
            temporary.write(data)
            temporary.write(b"\\n")
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_name, final_path)
    except Exception:
        try:
            os.unlink(temporary_name)
        except OSError:
            pass
        raise


def _connected_instance_inboxes(registry: Path) -> list[Path]:
    """Find private per-terminal inboxes for every currently live STREENOTE listener."""
    try:
        registry_stat = registry.lstat()
        if (
            not stat.S_ISDIR(registry_stat.st_mode)
            or registry_stat.st_uid != os.getuid()
            or stat.S_IMODE(registry_stat.st_mode) & 0o077
        ):
            return []
    except OSError:
        return []

    instance_ids = set()
    for manifest in registry.glob("*.json"):
        try:
            metadata = manifest.lstat()
            if (
                not stat.S_ISREG(metadata.st_mode)
                or metadata.st_uid != os.getuid()
                or stat.S_IMODE(metadata.st_mode) & 0o077
                or metadata.st_size > 4096
            ):
                continue
            registration = json.loads(manifest.read_text(encoding="utf-8"))
            if not isinstance(registration, dict):
                continue
            instance_id = registration.get("instance_id")
            if (
                registration.get("version") == 1
                and isinstance(instance_id, str)
                and manifest.name == f"{instance_id}.json"
                and _review_instance_is_active(registry, instance_id)
            ):
                instance_ids.add(instance_id)
        except (OSError, ValueError, TypeError, json.JSONDecodeError):
            continue

    inbox_root = registry / "inboxes"
    return [
        inbox_root / instance_id
        for instance_id in sorted(instance_ids)
    ]


def write_inbox_event(inbox: Path, event: dict) -> None:
    """Fan each Hermes hook event out to every live STREENOTE terminal.

    Every TUI gets a private queue, so one process cannot consume another
    terminal's messages. The shared Journal uses event IDs and an inter-process
    file lock to persist a single copy even though every process imports it.
    """
    registry_value = os.environ.get("STREENOTE_HERMES_REVIEW_SESSION_DIR")
    recipients = []
    if registry_value:
        registry = Path(registry_value).expanduser()
        if registry.is_absolute():
            recipients = _connected_instance_inboxes(registry)

    if not recipients:
        # Preserve the original single-inbox contract for non-SSH or legacy
        # Hermes integrations that do not publish a review-session registry.
        recipients = [inbox]

    for recipient in recipients:
        _write_inbox_event_one(recipient, event)


def write_history_snapshot(
    inbox: Path, session_id: str, conversation_id: str | None, history
) -> None:
    """Stage one stable history snapshot per Hermes session."""
    snapshot = history_snapshot_text(history)
    if not snapshot:
        return

    state = inbox / "state"
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        os.chmod(inbox, 0o700)
        os.chmod(state, 0o700)
    except OSError:
        pass

    session_key = hashlib.sha256(session_id.encode("utf-8")).hexdigest()
    marker = state / f"{session_key}.history-v1"
    if marker.exists():
        return

    event = {
        "version": 1,
        "event_id": make_event_id("history_snapshot", session_id, "initial"),
        "kind": "hermes_history",
        "timestamp": datetime.now(timezone.utc).astimezone().isoformat(
            timespec="seconds"
        ),
        "text": snapshot,
        "conversation_id": conversation_id,
        "session_id": session_id,
        "old_session_id": None,
        "turn_id": None,
        "tool_call_id": None,
        "iteration": None,
    }
    # Stage the durable event before setting the marker. A crash before the
    # marker only causes a retry of the same deduplicated event ID.
    write_inbox_event(inbox, event)
    try:
        fd = os.open(marker, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as marker_file:
            marker_file.write(event["event_id"] + "\n")
            marker_file.flush()
            os.fsync(marker_file.fileno())
    except FileExistsError:
        # Concurrent hooks may both stage the same snapshot. Rust-side event
        # ID deduplication makes the second copy harmless.
        pass


def session_event_text(event_name: str, payload: dict, extra: dict) -> str:
    labels = {
        "on_session_start": "Hermes session started.",
        "on_session_end": "Hermes turn finalized.",
        "on_session_finalize": "Hermes session finalized.",
        "on_session_reset": "Hermes session reset.",
        "post_compaction": "Hermes conversation context compacted.",
    }
    lines = [labels[event_name]]
    for key in (
        "reason",
        "exit_reason",
        "turn_exit_reason",
        "kind",
        "completed",
        "failed",
        "interrupted",
        "duration_seconds",
        "turns_completed",
        "turns_failed",
        "before_tokens",
        "after_tokens",
        "messages_before",
        "messages_after",
        "messages_compacted",
        "model",
        "platform",
    ):
        value = event_value(payload, extra, key)
        if isinstance(value, (str, int, float, bool)):
            lines.append(f"{key}: {redact_inline(str(value))}")
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
            "on_stream_start",
            "on_stream_delta",
            "on_stream_end",
            "on_interim_message",
            "on_session_start",
            "on_session_end",
            "on_session_finalize",
            "on_session_reset",
            "post_compaction",
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
        elif event_name == "post_tool_call":
            kind = "hermes_tool_result"
            text = tool_event_text(event_name, payload, extra)
        elif event_name == "on_stream_start":
            kind = "hermes_stream_start"
            text = "Assistant stream started."
        elif event_name == "on_stream_delta":
            # Capture user-visible text only. Reasoning deltas may be sensitive
            # and are intentionally excluded from the journal.
            if event_value(payload, extra, "kind") != "text":
                return 0
            kind = "hermes_stream_delta"
            text = event_value(payload, extra, "delta")
        elif event_name == "on_interim_message":
            kind = "hermes_interim"
            text = event_value(payload, extra, "text")
        elif event_name in (
            "on_session_start",
            "on_session_end",
            "on_session_finalize",
            "on_session_reset",
            "post_compaction",
        ):
            kind = "hermes_session"
            text = session_event_text(event_name, payload, extra)
        else:
            kind = "hermes_stream_end"
            finished = bool(event_value(payload, extra, "finished"))
            error = event_value(payload, extra, "error")
            text = "finished" if finished else "interrupted"
            if error:
                text += f"\nError: {redact_inline(str(error))}"

        if not isinstance(text, str):
            return 0
        # Whitespace-only text deltas are meaningful token boundaries; retain
        # them so separately streamed words do not get concatenated together.
        if event_name == "on_stream_delta":
            if not text:
                return 0
        elif not text.strip():
            return 0

        encoded_text = text.encode("utf-8")
        if len(encoded_text) > MAX_EVENT_BYTES:
            print(
                f"STREENOTE Hermes capture: event exceeds {MAX_EVENT_BYTES} bytes; skipped",
                file=sys.stderr,
            )
            return 0

        session_id = event_value(payload, extra, "session_id")
        conversation_id = event_value(payload, extra, "conversation_id")
        old_session_id = event_value(payload, extra, "old_session_id")
        if session_id is not None:
            session_id = str(session_id)
        if conversation_id is not None:
            conversation_id = str(conversation_id)
        if old_session_id is not None:
            old_session_id = str(old_session_id)
        turn_id = event_value(payload, extra, "turn_id")

        if turn_id is not None:
            turn_id = str(turn_id)
        if event_name == "pre_llm_call" and session_id:
            history = event_value(payload, extra, "conversation_history")
            if history:
                try:
                    write_history_snapshot(
                        Path(inbox_value).expanduser(),
                        session_id,
                        conversation_id,
                        history,
                    )
                except Exception as exc:
                    # A history snapshot is supplementary. Do not lose the
                    # current prompt if snapshot storage is unavailable.
                    print(
                        f"STREENOTE Hermes history snapshot failed: {exc}",
                        file=sys.stderr,
                    )
        tool_call_id = event_value(payload, extra, "tool_call_id")
        if tool_call_id is not None:
            tool_call_id = str(tool_call_id)
        iteration = event_value(payload, extra, "iteration")
        if iteration is not None:
            try:
                iteration = int(iteration)
                if iteration < 0 or iteration > 0xFFFFFFFF:
                    iteration = None
            except (TypeError, ValueError):
                iteration = None
        if event_name in ("pre_tool_call", "post_tool_call"):
            identity_id = tool_call_id
        elif event_name in ("on_stream_start", "on_stream_end"):
            identity_id = f"{turn_id or ''}:{iteration if iteration is not None else ''}"
        elif event_name in ("on_stream_delta", "on_interim_message", "post_compaction"):
            # These can occur multiple times within one turn or session.
            identity_id = None
        else:
            identity_id = turn_id
        event_id = make_event_id(event_name, session_id, identity_id)
        event = {
            "version": 1,
            "event_id": event_id,
            "kind": kind,
            "timestamp": datetime.now(timezone.utc).astimezone().isoformat(timespec="seconds"),
            "text": text,
            "conversation_id": conversation_id,
            "session_id": session_id,
            "old_session_id": old_session_id,
            "turn_id": turn_id,
            "tool_call_id": tool_call_id,
            "iteration": iteration,
        }

        inbox = Path(inbox_value).expanduser()
        write_inbox_event(inbox, event)

        return 0
    except Exception as exc:
        print(f"STREENOTE Hermes capture failed: {exc}", file=sys.stderr)
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
