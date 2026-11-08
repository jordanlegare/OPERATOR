"""Hermes final-response gate backed by the STREENOTE review socket.

The plugin holds a completed assistant response until STREENOTE releases the
original, returns an edited replacement, or rejects it. The request channel
uses a private Unix-domain socket shared by the Hermes gateway and STREENOTE.
"""

from __future__ import annotations

import json
import os
import socket
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

MAX_REVIEW_BYTES = 8 * 1024 * 1024
MAX_REPLY_WIRE_BYTES = (MAX_REVIEW_BYTES * 2) + 4096
SOCKET_TIMEOUT_SECONDS = 550
MAX_METADATA_CHARS = 1024


def _diagnostic_log_path() -> Path:
    explicit = os.environ.get("STREENOTE_HERMES_REVIEW_LOG")
    if explicit and Path(explicit).expanduser().is_absolute():
        return Path(explicit).expanduser()
    runtime_dir = Path("/run/user") / str(os.getuid())
    try:
        runtime_stat = runtime_dir.stat()
        if runtime_dir.is_dir() and runtime_stat.st_uid == os.getuid():
            return runtime_dir / "streenote" / "hermes-review.log"
    except OSError:
        pass
    state_home = os.environ.get("XDG_STATE_HOME")
    if state_home and Path(state_home).is_absolute():
        root = Path(state_home)
    else:
        root = Path.home() / ".local" / "state"
    return root / "streenote" / "hermes-review.log"


def _log_event(event: str, **fields: Any) -> None:
    """Append private, content-free diagnostics; logging must never break review."""
    try:
        path = _diagnostic_log_path()
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        try:
            os.chmod(path.parent, 0o700)
        except OSError:
            pass
        record = {"event": event, "timestamp": datetime.now(timezone.utc).isoformat(), **fields}
        payload = (json.dumps(record, ensure_ascii=True, separators=(",", ":")) + "\n").encode("utf-8")
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        try:
            try:
                os.fchmod(descriptor, 0o600)
            except OSError:
                pass
            os.write(descriptor, payload)
        finally:
            os.close(descriptor)
    except Exception:
        pass


def _review_required() -> bool:
    return os.environ.get("STREENOTE_HERMES_REVIEW_REQUIRED", "").strip().lower() in {
        "1", "true", "yes", "on"
    }

def runtime_review_socket_path(uid: int | None = None, runtime_root: Path = Path("/run/user")) -> Path | None:
    """Return a same-UID socket path despite different HOME/XDG environments."""
    uid = os.getuid() if uid is None else uid
    runtime_dir = runtime_root / str(uid)
    try:
        metadata = runtime_dir.stat()
    except OSError:
        return None
    if not runtime_dir.is_dir() or metadata.st_uid != uid:
        return None
    return runtime_dir / "streenote" / "review.sock"


def review_socket_path() -> Path:
    """Resolve the same per-user socket path used by the STREENOTE process."""
    explicit = os.environ.get("STREENOTE_HERMES_REVIEW_SOCKET")
    if explicit:
        candidate = Path(explicit).expanduser()
        if candidate.is_absolute():
            return candidate

    # The Hermes gateway and the STREENOTE PTY can have different HOME and
    # XDG_DATA_HOME values. /run/user/<uid> is shared by processes of this
    # Unix account and avoids that environment-dependent path mismatch.
    runtime_path = runtime_review_socket_path()
    if runtime_path is not None:
        return runtime_path

    xdg_data_home = os.environ.get("XDG_DATA_HOME")
    if xdg_data_home and Path(xdg_data_home).is_absolute():
        data_root = Path(xdg_data_home)
    else:
        data_root = Path.home() / ".local" / "share"
    return data_root / "streenote" / "review.sock"


def _read_json_line(connection: socket.socket) -> dict[str, Any]:
    received = bytearray()
    while b"\n" not in received:
        remaining = MAX_REPLY_WIRE_BYTES + 1 - len(received)
        if remaining <= 0:
            raise RuntimeError("STREENOTE review reply exceeded the size limit")
        chunk = connection.recv(min(65536, remaining))
        if not chunk:
            raise RuntimeError("STREENOTE closed the review channel without a decision")
        received.extend(chunk)

    line, _, trailing = received.partition(b"\n")
    if trailing.strip():
        raise RuntimeError("STREENOTE sent unexpected data after the review reply")
    if len(line) > MAX_REPLY_WIRE_BYTES:
        raise RuntimeError("STREENOTE review reply exceeded the size limit")
    try:
        decoded = json.loads(line.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"STREENOTE returned an invalid review reply: {error}") from error
    if not isinstance(decoded, dict):
        raise RuntimeError("STREENOTE review reply was not a JSON object")
    return decoded


def _exchange(path: Path, request: dict[str, Any]) -> dict[str, Any]:
    encoded = json.dumps(
        request, ensure_ascii=False, separators=(",", ":")
    ).encode("utf-8")
    if len(encoded) > MAX_REVIEW_BYTES:
        raise RuntimeError("Hermes final response exceeds STREENOTE's 8 MiB review limit")

    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(SOCKET_TIMEOUT_SECONDS)
        connection.connect(str(path))
        _log_event("SOCKET_CONNECT_OK", socket_path=str(path), request_id=request.get("request_id", ""))
        connection.sendall(encoded + b"\n")
        _log_event("REQUEST_SENT", request_id=request.get("request_id", ""))
        return _read_json_line(connection)


def review_response(
    response_text: str,
    session_id: str = "",
    model: str = "",
    platform: str = "",
    **kwargs: Any,
) -> str | None:
    """Hold the final reply until STREENOTE returns an explicit decision."""
    del kwargs
    required = _review_required()
    _log_event("CALLBACK_INVOKED", required=required, response_bytes=len(response_text.encode("utf-8")) if isinstance(response_text, str) else 0, platform=str(platform or "")[:128])
    if not isinstance(response_text, str) or not response_text:
        _log_event("EMPTY_RESPONSE_BYPASS")
        return None

    path = review_socket_path()
    exists = path.exists()
    _log_event("SOCKET_PATH_RESOLVED", socket_path=str(path), exists=exists, required=required)
    if not exists:
        _log_event("SOCKET_MISSING", socket_path=str(path), required=required)
        if required:
            raise RuntimeError(f"STREENOTE review is required but its socket is missing at {path}; the Hermes response was withheld.")
        return None

    request_id = uuid.uuid4().hex
    request = {
        "version": 1,
        "request_id": request_id,
        "session_id": str(session_id or "")[:MAX_METADATA_CHARS],
        "model": str(model or "")[:MAX_METADATA_CHARS],
        "platform": str(platform or "")[:128],
        "response_text": response_text,
    }

    try:
        reply = _exchange(path, request)
    except FileNotFoundError as error:
        _log_event("SOCKET_CONNECT_ERROR", socket_path=str(path), error=str(error), request_id=request_id)
        if required:
            raise RuntimeError(f"STREENOTE review is required but its socket disappeared at {path}; the Hermes response was withheld.") from error
        return None
    except ConnectionRefusedError as error:
        _log_event("SOCKET_CONNECT_ERROR", socket_path=str(path), error=str(error), request_id=request_id)
        raise RuntimeError("STREENOTE review socket is stale; the Hermes response was withheld. Restart STREENOTE to recover the review channel.") from error
    except (TimeoutError, socket.timeout) as error:
        _log_event("REVIEW_ERROR", request_id=request_id, error="socket timeout")
        raise RuntimeError("STREENOTE review timed out; the Hermes response was withheld.") from error
    except OSError as error:
        _log_event("REVIEW_ERROR", request_id=request_id, error=str(error))
        raise RuntimeError(f"STREENOTE review channel failed; the Hermes response was withheld: {error}") from error
    except RuntimeError as error:
        _log_event("REVIEW_ERROR", request_id=request_id, error=str(error))
        raise

    if reply.get("version") != 1:
        _log_event("REVIEW_ERROR", request_id=request_id, error="unsupported protocol version")
        raise RuntimeError("STREENOTE returned an unsupported review protocol version")

    decision = reply.get("decision")
    _log_event("DECISION_RECEIVED", request_id=request_id, decision=str(decision))
    if decision == "bypass":
        _log_event("REVIEW_BYPASS", request_id=request_id, required=required)
        if required:
            raise RuntimeError("STREENOTE review is required but STREENOTE returned bypass (review mode is OFF); the Hermes response was withheld.")
        return None
    if decision == "approve":
        _log_event("REVIEW_APPROVED", request_id=request_id)
        return response_text
    if decision == "replace":
        replacement = reply.get("response_text")
        if not isinstance(replacement, str) or not replacement.strip():
            _log_event("REVIEW_ERROR", request_id=request_id, error="empty replacement")
            raise RuntimeError("STREENOTE returned an empty replacement; the Hermes response was withheld.")
        if len(replacement.encode("utf-8")) > MAX_REVIEW_BYTES:
            _log_event("REVIEW_ERROR", request_id=request_id, error="replacement too large")
            raise RuntimeError("STREENOTE replacement exceeded the review limit; the Hermes response was withheld.")
        _log_event("REVIEW_REPLACED", request_id=request_id, replacement_bytes=len(replacement.encode("utf-8")))
        return replacement
    if decision == "reject":
        reason = reply.get("error")
        detail = f" ({reason})" if isinstance(reason, str) and reason else ""
        _log_event("REVIEW_REJECTED", request_id=request_id, reason=str(reason or "")[:300])
        raise RuntimeError(f"STREENOTE operator withheld this Hermes response{detail}.")
    _log_event("REVIEW_ERROR", request_id=request_id, error=f"unknown decision {decision!r}")
    raise RuntimeError(f"STREENOTE returned an unknown review decision {decision!r}; the response was withheld.")


def register(ctx: Any) -> None:
    # hold_stream is essential: without it, Hermes could display the original
    # draft before STREENOTE has had a chance to review or replace it.
    ctx.register_hook("transform_llm_output", review_response, hold_stream=True)
    _log_event("PLUGIN_REGISTERED", hook="transform_llm_output", hold_stream=True)
