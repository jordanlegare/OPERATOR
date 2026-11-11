"""Hermes final-response gate backed by the OPERATOR review socket.

The plugin holds a completed assistant response until OPERATOR releases the
original, returns an edited replacement, or rejects it. The request channel
uses a private Unix-domain socket shared by the Hermes gateway and OPERATOR.
"""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import re
import socket
import stat
import tempfile
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

MAX_REVIEW_BYTES = 8 * 1024 * 1024
MAX_REPLY_WIRE_BYTES = (MAX_REVIEW_BYTES * 2) + 4096
SOCKET_TIMEOUT_SECONDS = 550
MAX_METADATA_CHARS = 1024
MAX_PROMPT_BYTES = 8 * 1024 * 1024

_PROMPT_SECRET_ASSIGNMENT = re.compile(
    r"""(?i)\b(api[_-]?key|access[_-]?token|refresh[_-]?token|password|passwd|secret|authorization|credential)\b(\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s,;]+)"""
)
_PROMPT_BEARER_VALUE = re.compile(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+")
_PROMPT_SECRET_FLAG = re.compile(
    r"""(?i)(--(?:api[-_]key|token|password|secret)(?:=|\s+))(?:\"[^\"]*\"|'[^']*'|[^\s]+)"""
)


def _diagnostic_log_path() -> Path:
    explicit = os.environ.get("OPERATOR_HERMES_REVIEW_LOG")
    if explicit and Path(explicit).expanduser().is_absolute():
        return Path(explicit).expanduser()
    runtime_dir = Path("/run/user") / str(os.getuid())
    try:
        runtime_stat = runtime_dir.stat()
        if runtime_dir.is_dir() and runtime_stat.st_uid == os.getuid():
            return runtime_dir / "operator" / "hermes-review.log"
    except OSError:
        pass
    state_home = os.environ.get("XDG_STATE_HOME")
    if state_home and Path(state_home).is_absolute():
        root = Path(state_home)
    else:
        root = Path.home() / ".local" / "state"
    return root / "operator" / "hermes-review.log"


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
    return os.environ.get("OPERATOR_HERMES_REVIEW_REQUIRED", "").strip().lower() in {
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
    return runtime_dir / "operator" / "review.sock"


def review_socket_path() -> Path:
    """Resolve the same per-user socket path used by the OPERATOR process."""
    explicit = os.environ.get("OPERATOR_HERMES_REVIEW_SOCKET")
    if explicit:
        candidate = Path(explicit).expanduser()
        if candidate.is_absolute():
            return candidate

    # The Hermes gateway and the OPERATOR PTY can have different HOME and
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
    return data_root / "operator" / "review.sock"

def review_session_registry_dir() -> Path:
    """Directory containing one socket/manifest pair per live OPERATOR TUI."""
    explicit = os.environ.get("OPERATOR_HERMES_REVIEW_SESSION_DIR")
    if explicit:
        candidate = Path(explicit).expanduser()
        if candidate.is_absolute():
            return candidate
    return review_socket_path().parent / "review-sessions"


def _owned_unix_socket(path: Path) -> bool:
    try:
        metadata = path.lstat()
    except OSError:
        return False
    return (
        stat.S_ISSOCK(metadata.st_mode)
        and metadata.st_uid == os.getuid()
        and not (stat.S_IMODE(metadata.st_mode) & 0o077)
    )


def _load_review_routes() -> tuple[list[dict[str, Any]], Path]:
    """Load registered per-instance sockets plus the legacy single socket."""
    directory = review_session_registry_dir()
    routes: list[dict[str, Any]] = []
    registry_is_private = False
    try:
        directory_stat = directory.lstat()
        registry_is_private = (
            stat.S_ISDIR(directory_stat.st_mode)
            and directory_stat.st_uid == os.getuid()
            and not (stat.S_IMODE(directory_stat.st_mode) & 0o077)
        )
    except OSError:
        pass

    if registry_is_private:
        for manifest in sorted(directory.glob("*.json")):
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
                    registration.get("version") != 1
                    or not isinstance(instance_id, str)
                    or not re.fullmatch(r"[a-f0-9-]{1,64}", instance_id)
                    or manifest.name != f"{instance_id}.json"
                ):
                    continue
                socket_path = directory / f"{instance_id}.sock"
                if not _owned_unix_socket(socket_path):
                    # A dead process may have exited before it could unregister.
                    try:
                        manifest.unlink()
                    except OSError:
                        pass
                    continue
                registered_path = registration.get("socket_path")
                if registered_path and Path(registered_path) != socket_path:
                    continue
                routes.append({
                    "instance_id": instance_id,
                    "socket_path": socket_path,
                    "manifest_path": manifest,
                    "review_enabled": bool(registration.get("review_enabled", True)),
                })
            except (OSError, ValueError, TypeError, json.JSONDecodeError):
                continue

    # Preserve compatibility with older OPERATOR binaries and single-socket
    # unit-test/standalone deployments, even when no new registry exists yet.
    legacy_path = review_socket_path()
    if _owned_unix_socket(legacy_path) and all(
        route["socket_path"] != legacy_path for route in routes
    ):
        routes.append({
            "instance_id": "legacy",
            "socket_path": legacy_path,
            "manifest_path": None,
            "review_enabled": True,
        })
    return routes, directory


def _write_dispatch_cursor(path: Path, instance_id: str) -> None:
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(descriptor, (instance_id + "\n").encode("utf-8"))
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
    os.replace(temporary, path)
    os.chmod(path, 0o600)


def _refresh_review_routes(
    routes: list[dict[str, Any]], registry_dir: Path
) -> list[dict[str, Any]]:
    """Refresh registry-backed routes so live /review toggles take effect while queued."""
    if any(route.get("manifest_path") is not None for route in routes):
        fresh_routes, fresh_registry_dir = _load_review_routes()
        if fresh_registry_dir == registry_dir:
            return fresh_routes
    # Legacy listeners and isolated scheduler tests have no per-instance
    # manifest. Preserve their supplied route list for compatibility.
    return routes


def _acquire_review_slot(
    routes: list[dict[str, Any]],
    registry_dir: Path,
    timeout: float = SOCKET_TIMEOUT_SECONDS,
    excluded: set[str] | None = None,
) -> tuple[dict[str, Any], int] | None:
    """Acquire one currently enabled listener's slot using live round-robin routing.

    Returns None when every live terminal has opted out. While eligible
    listeners are busy, refresh their manifests every poll so /review off
    withdraws a terminal immediately and /review on can add it to the pool.
    flock is released automatically if the gateway callback crashes.
    """
    excluded = excluded or set()
    dispatch_dir = registry_dir / "dispatch"
    dispatch_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        os.chmod(dispatch_dir, 0o700)
    except OSError:
        pass
    scheduler_path = dispatch_dir / "scheduler.lock"
    cursor_path = dispatch_dir / "round-robin"
    deadline = time.monotonic() + max(0.0, timeout)
    registry_backed = any(route.get("manifest_path") is not None for route in routes)

    while True:
        current_routes = (
            _refresh_review_routes(routes, registry_dir)
            if registry_backed
            else routes
        )
        candidates = sorted(
            (
                route for route in current_routes
                if route.get("review_enabled", True)
                and route["instance_id"] not in excluded
            ),
            key=lambda route: route["instance_id"],
        )
        if not candidates:
            return None

        scheduler_fd = os.open(scheduler_path, os.O_CREAT | os.O_RDWR, 0o600)
        selected: tuple[dict[str, Any], int] | None = None
        try:
            os.fchmod(scheduler_fd, 0o600)
            fcntl.flock(scheduler_fd, fcntl.LOCK_EX)
            try:
                last_id = cursor_path.read_text(encoding="utf-8").strip()
            except OSError:
                last_id = ""
            start = 0
            for index, route in enumerate(candidates):
                if route["instance_id"] == last_id:
                    start = (index + 1) % len(candidates)
                    break
            ordered = candidates[start:] + candidates[:start]

            for route in ordered:
                lock_path = dispatch_dir / f"{route['instance_id']}.lock"
                lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o600)
                try:
                    os.fchmod(lock_fd, 0o600)
                    fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    os.close(lock_fd)
                    continue
                try:
                    _write_dispatch_cursor(cursor_path, route["instance_id"])
                except Exception:
                    _release_review_slot(lock_fd)
                    raise
                selected = (route, lock_fd)
                break
        finally:
            fcntl.flock(scheduler_fd, fcntl.LOCK_UN)
            os.close(scheduler_fd)

        if selected is not None:
            return selected
        if time.monotonic() >= deadline:
            raise TimeoutError(
                "All enabled OPERATOR reviewers remained busy until the review queue deadline."
            )
        time.sleep(0.05)

def _release_review_slot(lock_fd: int) -> None:
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
    finally:
        os.close(lock_fd)


def _read_json_line(connection: socket.socket) -> dict[str, Any]:
    received = bytearray()
    while b"\n" not in received:
        remaining = MAX_REPLY_WIRE_BYTES + 1 - len(received)
        if remaining <= 0:
            raise RuntimeError("OPERATOR review reply exceeded the size limit")
        chunk = connection.recv(min(65536, remaining))
        if not chunk:
            raise RuntimeError("OPERATOR closed the review channel without a decision")
        received.extend(chunk)

    line, _, trailing = received.partition(b"\n")
    if trailing.strip():
        raise RuntimeError("OPERATOR sent unexpected data after the review reply")
    if len(line) > MAX_REPLY_WIRE_BYTES:
        raise RuntimeError("OPERATOR review reply exceeded the size limit")
    try:
        decoded = json.loads(line.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"OPERATOR returned an invalid review reply: {error}") from error
    if not isinstance(decoded, dict):
        raise RuntimeError("OPERATOR review reply was not a JSON object")
    return decoded


class _ReviewSocketUnavailable(Exception):
    """A route disappeared before a connection was established."""


def _exchange(
    path: Path,
    request: dict[str, Any],
    instance_id: str,
    observe_only: bool = False,
    phase: str = "pending",
    decision: str | None = None,
) -> dict[str, Any]:
    payload = dict(request)
    payload["observe_only"] = observe_only
    payload["phase"] = phase
    payload["decision"] = decision
    encoded = json.dumps(
        payload, ensure_ascii=False, separators=(",", ":")
    ).encode("utf-8")
    if len(encoded) > MAX_REVIEW_BYTES:
        raise RuntimeError("Hermes final response exceeds OPERATOR's 8 MiB review limit")

    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.settimeout(SOCKET_TIMEOUT_SECONDS)
    try:
        try:
            connection.connect(str(path))
        except (FileNotFoundError, ConnectionRefusedError) as error:
            # Only a failure before connect succeeds is safe to reroute.
            # Once a peer has accepted the request, never duplicate a
            # potentially displayed/approved review to a second terminal.
            raise _ReviewSocketUnavailable(str(error)) from error
        _log_event(
            "SOCKET_CONNECT_OK",
            socket_path=str(path),
            instance_id=instance_id,
            request_id=request.get("request_id", ""),
        )
        with connection:
            connection.sendall(encoded + b"\n")
            _log_event(
                "REQUEST_SENT",
                request_id=request.get("request_id", ""),
                instance_id=instance_id,
            )
            return _read_json_line(connection)
    except Exception:
        connection.close()
        raise


def _render_prompt_text(value: Any) -> str | None:
    """Render original user text while replacing multimodal payloads with labels."""
    def redact(text: str) -> str:
        text = _PROMPT_SECRET_ASSIGNMENT.sub(
            lambda match: f"{match.group(1)}{match.group(2)}[REDACTED]", text
        )
        text = _PROMPT_BEARER_VALUE.sub("Bearer [REDACTED]", text)
        return _PROMPT_SECRET_FLAG.sub(lambda match: f"{match.group(1)}[REDACTED]", text)

    if isinstance(value, str):
        return redact(value)
    if isinstance(value, dict):
        if isinstance(value.get("text"), str):
            return redact(value["text"])
        if "content" in value:
            return _render_prompt_text(value["content"])
        kind = str(value.get("type") or "").lower()
        if "image" in kind or "image_url" in value:
            return "[image attachment]"
        if "audio" in kind:
            return "[audio attachment]"
        if "file" in kind:
            return "[file attachment]"
        return None
    if isinstance(value, list):
        parts = []
        for item in value:
            if isinstance(item, str):
                parts.append(redact(item))
                continue
            if not isinstance(item, dict):
                continue
            kind = str(item.get("type") or "").lower()
            text_value = item.get("text")
            if isinstance(text_value, str) and (
                kind in {"text", "input_text", "output_text"} or not kind
            ):
                parts.append(redact(text_value))
            elif "image" in kind or "image_url" in item:
                parts.append("[image attachment]")
            elif "audio" in kind:
                parts.append("[audio attachment]")
            elif "file" in kind:
                parts.append("[file attachment]")
        return "\n".join(parts) if parts else None
    return None



def _last_user_message_from_history(history: Any) -> Any:
    """Fallback for hook implementations that omit user_message."""
    if isinstance(history, dict):
        history = history.get("messages")
    if not isinstance(history, list):
        return None
    for message in reversed(history):
        if not isinstance(message, dict):
            continue
        role = str(message.get("role") or message.get("type") or "").strip().lower()
        if role not in {"user", "human"}:
            continue
        content = message.get("content")
        if content is None:
            content = message.get("text")
        if content is not None:
            return content
    return None


def _default_prompt_inbox() -> Path:
    explicit = os.environ.get("OPERATOR_HERMES_INBOX")
    if explicit and Path(explicit).expanduser().is_absolute():
        return Path(explicit).expanduser()
    data_home = os.environ.get("XDG_DATA_HOME")
    if data_home and Path(data_home).expanduser().is_absolute():
        return Path(data_home).expanduser() / "operator" / "hermes-inbox"
    return Path.home() / ".local" / "share" / "operator" / "hermes-inbox"


def _write_prompt_event(inbox: Path, event: dict[str, Any]) -> None:
    ready = inbox / "ready"
    ready.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        os.chmod(inbox, 0o700)
        os.chmod(ready, 0o700)
    except OSError:
        pass

    final_path = ready / f"{time.time_ns():020d}-{event['event_id']}.json"
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=".operator-prompt-", suffix=".tmp", dir=ready
    )
    try:
        with os.fdopen(descriptor, "wb") as temporary:
            os.fchmod(temporary.fileno(), 0o600)
            temporary.write(
                json.dumps(event, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
            )
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


def capture_user_prompt(
    source: str = "pre_llm_call",
    session_id: str = "",
    user_message: Any = None,
    turn_id: str | None = None,
    **kwargs: Any,
) -> None:
    """Fan the original user message out through a native plugin observer.

    Hermes dispatches plugin callbacks as keyword arguments. Keep this callback
    fail-open and content-free in its diagnostics; multiple hook sources use
    the same stable event ID so a prompt can be recovered when one hook path is
    absent without creating duplicate journal rows.
    """
    session_id = str(session_id or "")
    turn_id = str(turn_id) if turn_id is not None else None
    history = kwargs.get("conversation_history")
    is_first_turn = kwargs.get("is_first_turn")
    try:
        rendered = _render_prompt_text(user_message)
        message_source = "user_message"
        if not isinstance(rendered, str) or not rendered.strip():
            fallback_message = _last_user_message_from_history(history)
            rendered = _render_prompt_text(fallback_message)
            message_source = "conversation_history"

        _log_event(
            "PROMPT_HOOK_INVOKED",
            hook=source,
            user_message_type=type(user_message).__name__,
            history_type=type(history).__name__,
            is_first_turn=bool(is_first_turn),
            has_turn_id=bool(turn_id),
            session_hash=hashlib.sha256(session_id.encode("utf-8")).hexdigest()[:16],
            captured=bool(isinstance(rendered, str) and rendered.strip()),
            message_source=message_source if isinstance(rendered, str) and rendered.strip() else "none",
        )

        if not isinstance(rendered, str) or not rendered.strip():
            _log_event(
                "PROMPT_CAPTURE_SKIPPED",
                hook=source,
                reason="no_user_message_in_hook_payload",
                session_hash=hashlib.sha256(session_id.encode("utf-8")).hexdigest()[:16],
            )
            return
        encoded = rendered.encode("utf-8")
        if len(encoded) > MAX_PROMPT_BYTES:
            _log_event(
                "PROMPT_CAPTURE_SKIPPED",
                hook=source,
                reason="prompt_exceeds_size_limit",
                prompt_bytes=len(encoded),
            )
            return

        conversation_id = kwargs.get("conversation_id")
        if conversation_id is not None:
            conversation_id = str(conversation_id)

        if turn_id:
            identity = f"pre_llm_call\0{session_id}\0{turn_id}"
            event_id = hashlib.sha256(identity.encode("utf-8")).hexdigest()
        else:
            # Use a per-session/content fingerprint when an older hook payload
            # omits turn_id. This de-duplicates multiple observers for the same
            # prompt without persisting the message text in diagnostics.
            fingerprint = hashlib.sha256(encoded).hexdigest()
            event_id = hashlib.sha256(
                f"prompt\0{session_id}\0{fingerprint}".encode("utf-8")
            ).hexdigest()

        event = {
            "version": 1,
            "event_id": event_id,
            "kind": "hermes_user",
            "timestamp": datetime.now(timezone.utc).astimezone().isoformat(timespec="seconds"),
            "text": rendered,
            "conversation_id": conversation_id,
            "session_id": session_id or None,
            "old_session_id": None,
            "turn_id": turn_id,
            "tool_call_id": None,
            "iteration": None,
        }

        routes, registry_dir = _load_review_routes()
        recipients: dict[Path, Path] = {}
        fallback = _default_prompt_inbox()
        for route in routes:
            instance_id = route.get("instance_id")
            if instance_id == "legacy" or route.get("manifest_path") is None:
                recipient = fallback
            else:
                recipient = registry_dir / "inboxes" / str(instance_id)
            normalized = recipient.expanduser().resolve(strict=False)
            recipients.setdefault(normalized, normalized)

        if not recipients and fallback.is_absolute():
            recipients[fallback.resolve(strict=False)] = fallback.resolve(strict=False)

        if not recipients:
            _log_event(
                "PROMPT_CAPTURE_SKIPPED",
                hook=source,
                reason="no_live_terminal_inboxes",
                session_hash=hashlib.sha256(session_id.encode("utf-8")).hexdigest()[:16],
            )
            return

        delivered = 0
        failures = []
        for inbox in recipients.values():
            try:
                _write_prompt_event(inbox, event)
                delivered += 1
            except Exception as error:
                failures.append(str(error)[:300])

        _log_event(
            "PROMPT_FANOUT",
            hook=source,
            event_id=event_id,
            delivered=delivered,
            recipients=len(recipients),
            session_hash=hashlib.sha256(session_id.encode("utf-8")).hexdigest()[:16],
            turn_id=turn_id or "",
            message_source=message_source,
            registry=str(registry_dir),
            instance_ids=[route["instance_id"] for route in routes],
            failures=failures,
        )
    except Exception as error:
        # Prompt capture is observational only and must never alter model flow.
        _log_event(
            "PROMPT_CAPTURE_FAILED",
            hook=source,
            error=str(error)[:300],
            session_hash=hashlib.sha256(session_id.encode("utf-8")).hexdigest()[:16],
        )


def capture_pre_llm_prompt(**kwargs: Any) -> None:
    capture_user_prompt(source="pre_llm_call", **kwargs)


def capture_pre_api_prompt(**kwargs: Any) -> None:
    capture_user_prompt(source="pre_api_request", **kwargs)


def capture_post_llm_prompt(**kwargs: Any) -> None:
    capture_user_prompt(source="post_llm_call", **kwargs)


def review_response(
    response_text: str,
    session_id: str = "",
    model: str = "",
    platform: str = "",
    **kwargs: Any,
) -> str | None:
    """Assign each final reply to one active, currently available reviewer."""
    del kwargs
    required = _review_required()
    _log_event(
        "CALLBACK_INVOKED",
        required=required,
        response_bytes=len(response_text.encode("utf-8")) if isinstance(response_text, str) else 0,
        platform=str(platform or "")[:128],
    )
    if not isinstance(response_text, str) or not response_text:
        _log_event("EMPTY_RESPONSE_BYPASS")
        return None

    routes, registry_dir = _load_review_routes()
    _log_event(
        "SOCKET_ROUTES_RESOLVED",
        registry_dir=str(registry_dir),
        route_count=len(routes),
        instance_ids=[route["instance_id"] for route in routes],
        enabled_reviewers=[route["instance_id"] for route in routes if route.get("review_enabled", True)],
        required=required,
    )
    if not routes:
        legacy_path = review_socket_path()
        _log_event("SOCKET_MISSING", socket_path=str(legacy_path), required=required)
        if required:
            raise RuntimeError(
                f"OPERATOR review is required but no active review socket is registered "
                f"under {registry_dir}; the Hermes response was withheld."
            )
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

    # Dispatch only to an opted-in, currently available reviewer. Observer
    # terminals are not contacted; their journal event fan-out is independent
    # of review assignment.
    excluded: set[str] = set()
    reply: dict[str, Any] | None = None
    selected_route: dict[str, Any] | None = None

    while True:
        try:
            acquired = _acquire_review_slot(
                routes, registry_dir, excluded=excluded
            )
        except TimeoutError as error:
            _log_event(
                "REVIEW_POOL_BUSY",
                request_id=request_id,
                error=str(error),
                required=required,
            )
            raise RuntimeError(
                "All active OPERATOR reviewers are busy; the Hermes response was withheld."
            ) from error

        if acquired is None:
            selected_route = None
            _log_event(
                "REVIEW_BYPASS",
                request_id=request_id,
                required=required,
                reason="no active reviewers",
            )
            break

        selected_route, slot_fd = acquired
        try:
            try:
                reply = _exchange(
                    selected_route["socket_path"],
                    request,
                    selected_route["instance_id"],
                )
            except _ReviewSocketUnavailable as error:
                _log_event(
                    "REVIEWER_UNAVAILABLE",
                    request_id=request_id,
                    instance_id=selected_route["instance_id"],
                    error=str(error),
                )
                excluded.add(selected_route["instance_id"])
                manifest = selected_route.get("manifest_path")
                if manifest is not None:
                    try:
                        manifest.unlink()
                    except OSError:
                        pass
                try:
                    selected_route["socket_path"].unlink()
                except OSError:
                    pass
                routes = [
                    route for route in routes
                    if route["instance_id"] != selected_route["instance_id"]
                ]
                if not routes:
                    raise RuntimeError(
                        "All registered OPERATOR review sockets are stale or unavailable; "
                        "the Hermes response was withheld."
                    ) from error
                selected_route = None
                reply = None
                continue
        except (TimeoutError, socket.timeout) as error:
            _log_event("REVIEW_ERROR", request_id=request_id, error="socket timeout")
            raise RuntimeError(
                "OPERATOR review timed out; the Hermes response was withheld."
            ) from error
        except OSError as error:
            _log_event("REVIEW_ERROR", request_id=request_id, error=str(error))
            raise RuntimeError(
                f"OPERATOR review channel failed; the Hermes response was withheld: {error}"
            ) from error
        finally:
            _release_review_slot(slot_fd)

        if reply and reply.get("decision") == "bypass":
            # The operator may have opted out after the dispatcher read the
            # manifest but before the socket accepted this request. Do not
            # assign to that terminal again; refresh and try another reviewer.
            excluded.add(selected_route["instance_id"])
            _log_event(
                "REVIEWER_OPTED_OUT_DURING_DISPATCH",
                request_id=request_id,
                instance_id=selected_route["instance_id"],
            )
            selected_route = None
            reply = None
            continue
        break

    if selected_route is None:
        # Every connected terminal has explicitly opted out (or became
        # unavailable while this request was queued). This deliberate operator
        # choice bypasses even OPERATOR_HERMES_REVIEW_REQUIRED.
        return None

    if reply is None:
        raise RuntimeError(
            "No connected OPERATOR reviewer was available; the Hermes response was withheld."
        )

    session_hash = hashlib.sha256(str(session_id or "").encode("utf-8")).hexdigest()[:16]
    _log_event(
        "SESSION_REVIEW_ASSIGNED",
        request_id=request_id,
        session_hash=session_hash,
        instance_id=selected_route["instance_id"],
        socket_path=str(selected_route["socket_path"]),
    )

    if reply.get("version") != 1:
        _log_event("REVIEW_ERROR", request_id=request_id, error="unsupported protocol version")
        raise RuntimeError("OPERATOR returned an unsupported review protocol version")

    decision = reply.get("decision")
    _log_event(
        "DECISION_RECEIVED",
        request_id=request_id,
        decision=str(decision),
        instance_id=selected_route["instance_id"],
    )
    if decision == "bypass":
        # A local race may still reach this path after the reviewer toggles off.
        _log_event("REVIEW_BYPASS", request_id=request_id, required=required)
        return None
    if decision == "approve":
        _log_event("REVIEW_APPROVED", request_id=request_id)
        return response_text
    if decision == "replace":
        replacement = reply.get("response_text")
        if not isinstance(replacement, str) or not replacement.strip():
            _log_event("REVIEW_ERROR", request_id=request_id, error="empty replacement")
            raise RuntimeError(
                "OPERATOR returned an empty replacement; the Hermes response was withheld."
            )
        if len(replacement.encode("utf-8")) > MAX_REVIEW_BYTES:
            _log_event("REVIEW_ERROR", request_id=request_id, error="replacement too large")
            raise RuntimeError(
                "OPERATOR replacement exceeded the review limit; the Hermes response was withheld."
            )
        _log_event(
            "REVIEW_REPLACED",
            request_id=request_id,
            replacement_bytes=len(replacement.encode("utf-8")),
        )
        return replacement
    if decision == "reject":
        reason = reply.get("error")
        detail = f" ({reason})" if isinstance(reason, str) and reason else ""
        _log_event("REVIEW_REJECTED", request_id=request_id, reason=str(reason or "")[:300])
        raise RuntimeError(f"OPERATOR operator withheld this Hermes response{detail}.")
    _log_event("REVIEW_ERROR", request_id=request_id, error=f"unknown decision {decision!r}")
    raise RuntimeError(
        f"OPERATOR returned an unknown review decision {decision!r}; the response was withheld."
    )


def register(ctx: Any) -> None:
    # The pre-LLM hook is the immediate path. The per-provider observer and
    # successful-turn observer are independent fallbacks for gateway/provider
    # paths where the pre-LLM payload is incomplete or not dispatched.
    ctx.register_hook("pre_llm_call", capture_pre_llm_prompt)
    ctx.register_hook("pre_api_request", capture_pre_api_prompt)
    ctx.register_hook("post_llm_call", capture_post_llm_prompt)
    # hold_stream is essential: without it, Hermes could display the original
    # draft before OPERATOR has had a chance to review or replace it.
    ctx.register_hook("transform_llm_output", review_response, hold_stream=True)
    _log_event(
        "PLUGIN_REGISTERED",
        hooks=[
            "pre_llm_call",
            "pre_api_request",
            "post_llm_call",
            "transform_llm_output",
        ],
        hold_stream=True,
    )
