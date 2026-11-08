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
from pathlib import Path
from typing import Any

MAX_REVIEW_BYTES = 8 * 1024 * 1024
MAX_REPLY_WIRE_BYTES = MAX_REVIEW_BYTES + 4096
SOCKET_TIMEOUT_SECONDS = 550
MAX_METADATA_CHARS = 1024


def review_socket_path() -> Path:
    """Resolve the same per-user socket path used by the STREENOTE process."""
    explicit = os.environ.get("STREENOTE_HERMES_REVIEW_SOCKET")
    if explicit:
        candidate = Path(explicit).expanduser()
        if candidate.is_absolute():
            return candidate

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
        connection.sendall(encoded + b"\n")
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
    if not isinstance(response_text, str) or not response_text:
        return None

    path = review_socket_path()
    # No active STREENOTE instance means there is no operator to review this
    # turn. Do not strand ordinary Hermes sessions that are not STREENOTE-owned.
    if not path.exists():
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
    except FileNotFoundError:
        # STREENOTE may have exited between the exists check and connect.
        return None
    except ConnectionRefusedError as error:
        # A stale pathname is not treated as an inactive review session. This
        # prevents silently sending a draft when a configured listener broke.
        raise RuntimeError(
            "STREENOTE review socket is stale; the Hermes response was withheld. "
            "Restart STREENOTE to recover the review channel."
        ) from error
    except (TimeoutError, socket.timeout) as error:
        raise RuntimeError(
            "STREENOTE review timed out; the Hermes response was withheld."
        ) from error
    except OSError as error:
        raise RuntimeError(
            f"STREENOTE review channel failed; the Hermes response was withheld: {error}"
        ) from error

    if reply.get("version") != 1:
        raise RuntimeError("STREENOTE returned an unsupported review protocol version")

    decision = reply.get("decision")
    if decision == "bypass":
        return None
    if decision == "approve":
        # Return the exact approved text as a non-empty transform result so a
        # later transform hook cannot silently rewrite what the operator saw.
        return response_text

    if decision == "replace":
        replacement = reply.get("response_text")
        if not isinstance(replacement, str) or not replacement.strip():
            raise RuntimeError(
                "STREENOTE returned an empty replacement; the Hermes response was withheld."
            )
        if len(replacement.encode("utf-8")) > MAX_REVIEW_BYTES:
            raise RuntimeError(
                "STREENOTE replacement exceeded the review limit; the Hermes response was withheld."
            )
        return replacement

    if decision == "reject":
        reason = reply.get("error")
        detail = f" ({reason})" if isinstance(reason, str) and reason else ""
        raise RuntimeError(
            f"STREENOTE operator withheld this Hermes response{detail}."
        )

    raise RuntimeError(
        f"STREENOTE returned an unknown review decision {decision!r}; "
        "the Hermes response was withheld."
    )


def register(ctx: Any) -> None:
    # hold_stream is essential: without it, Hermes could display the original
    # draft before STREENOTE has had a chance to review or replace it.
    ctx.register_hook("transform_llm_output", review_response, hold_stream=True)
