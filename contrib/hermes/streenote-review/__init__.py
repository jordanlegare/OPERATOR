"""Hermes final-response gate backed by the STREENOTE review socket.

The plugin holds a completed assistant response until STREENOTE releases the
original, returns an edited replacement, or rejects it. The request channel
uses a private Unix-domain socket shared by the Hermes gateway and STREENOTE.
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

def review_session_registry_dir() -> Path:
    """Directory containing one socket/manifest pair per live STREENOTE TUI."""
    explicit = os.environ.get("STREENOTE_HERMES_REVIEW_SESSION_DIR")
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

    # Preserve compatibility with older STREENOTE binaries and single-socket
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


def _acquire_review_slot(
    routes: list[dict[str, Any]],
    registry_dir: Path,
    timeout: float = SOCKET_TIMEOUT_SECONDS,
    excluded: set[str] | None = None,
) -> tuple[dict[str, Any], int]:
    """Acquire one enabled listener's process-wide slot, round-robin.

    flock is automatically released if the Hermes gateway dies, so a crashed
    callback cannot permanently consume a reviewer's capacity. Each listener
    handles one active review at a time; concurrent answers wait for a free
    listener instead of failing because every TUI is momentarily occupied.
    """
    excluded = excluded or set()
    candidates = sorted(
        (
            route for route in routes
            if route.get("review_enabled", True)
            and route["instance_id"] not in excluded
        ),
        key=lambda route: route["instance_id"],
    )
    if not candidates:
        raise RuntimeError("No connected STREENOTE terminal currently has /review on.")

    dispatch_dir = registry_dir / "dispatch"
    dispatch_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        os.chmod(dispatch_dir, 0o700)
    except OSError:
        pass
    scheduler_path = dispatch_dir / "scheduler.lock"
    cursor_path = dispatch_dir / "round-robin"
    deadline = time.monotonic() + max(0.0, timeout)

    while True:
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
            raise RuntimeError(
                "All connected STREENOTE reviewers are busy; the Hermes response was withheld."
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
        raise RuntimeError("Hermes final response exceeds STREENOTE's 8 MiB review limit")

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

def _broadcast_review_result(
    routes: list[dict[str, Any]],
    request: dict[str, Any],
    decision: str,
    final_response: str | None,
) -> None:
    """Publish the final outcome to every connected terminal, including reviewer."""
    result_request = dict(request)
    result_request["response_text"] = final_response or ""
    for route in routes:
        try:
            reply = _exchange(
                route["socket_path"],
                result_request,
                route["instance_id"],
                observe_only=True,
                phase="result",
                decision=decision,
            )
            if reply.get("decision") != "observed":
                raise RuntimeError("terminal did not acknowledge final-result broadcast")
            _log_event(
                "REVIEW_RESULT_BROADCAST",
                request_id=request["request_id"],
                instance_id=route["instance_id"],
                decision=decision,
            )
        except Exception as error:
            _log_event(
                "REVIEW_RESULT_BROADCAST_ERROR",
                request_id=request["request_id"],
                instance_id=route["instance_id"],
                decision=decision,
                error=str(error),
            )



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


def _default_prompt_inbox() -> Path:
    explicit = os.environ.get("STREENOTE_HERMES_INBOX")
    if explicit and Path(explicit).expanduser().is_absolute():
        return Path(explicit).expanduser()
    data_home = os.environ.get("XDG_DATA_HOME")
    if data_home and Path(data_home).expanduser().is_absolute():
        return Path(data_home).expanduser() / "streenote" / "hermes-inbox"
    return Path.home() / ".local" / "share" / "streenote" / "hermes-inbox"


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
        prefix=".streenote-prompt-", suffix=".tmp", dir=ready
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
    session_id: str = "",
    user_message: Any = None,
    turn_id: str | None = None,
    **kwargs: Any,
) -> None:
    """Fan the original pre-LLM user message to every connected STREENOTE inbox.

    This runs inside the already-loaded review plugin, not as a shell-hook
    subprocess. That avoids losing the first prompt to gateway hook startup or
    environment inheritance differences. The shell capture hook remains a
    redundant path and continues to stage history snapshots and other events.
    """
    try:
        rendered = _render_prompt_text(user_message)
        if not isinstance(rendered, str) or not rendered.strip():
            return
        if len(rendered.encode("utf-8")) > MAX_PROMPT_BYTES:
            _log_event(
                "PROMPT_CAPTURE_SKIPPED",
                reason="prompt_exceeds_size_limit",
                prompt_bytes=len(rendered.encode("utf-8")),
            )
            return

        session_id = str(session_id or "")
        turn_id = str(turn_id) if turn_id is not None else None
        conversation_id = kwargs.get("conversation_id")
        if conversation_id is not None:
            conversation_id = str(conversation_id)

        if turn_id:
            identity = f"pre_llm_call\0{session_id}\0{turn_id}"
            event_id = hashlib.sha256(identity.encode("utf-8")).hexdigest()
        else:
            # Hermes normally supplies turn_id. Preserve capture for older
            # hook payloads where it is absent; the observer must never block.
            event_id = uuid.uuid4().hex

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

        # A direct CLI invocation may have an inbox configured even when the
        # legacy single socket is not represented in the instance registry.
        if not recipients and fallback.is_absolute():
            recipients[fallback.resolve(strict=False)] = fallback.resolve(strict=False)

        if not recipients:
            _log_event(
                "PROMPT_CAPTURE_SKIPPED",
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
            event_id=event_id,
            delivered=delivered,
            recipients=len(recipients),
            session_hash=hashlib.sha256(session_id.encode("utf-8")).hexdigest()[:16],
            turn_id=turn_id or "",
            failures=failures,
        )
    except Exception as error:
        # Prompt capture is observational only and must never alter model flow.
        _log_event("PROMPT_CAPTURE_FAILED", error=str(error)[:300])


def review_response(
    response_text: str,
    session_id: str = "",
    model: str = "",
    platform: str = "",
    **kwargs: Any,
) -> str | None:
    """Broadcast each final reply to every terminal and assign one reviewer."""
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
                f"STREENOTE review is required but no active review socket is registered "
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

    # Only terminals with /review on are eligible to make the release
    # decision. Every other connected terminal still receives a read-only
    # copy, so the shared conversation remains visible across SSH windows.
    excluded: set[str] = set()
    reply: dict[str, Any] | None = None
    selected_route: dict[str, Any] | None = None
    reviewers_were_configured = any(
        route.get("review_enabled", True) for route in routes
    )
    max_attempts = max(1, len(routes))
    for _ in range(max_attempts):
        try:
            selected_route, slot_fd = _acquire_review_slot(
                routes, registry_dir, excluded=excluded
            )
        except RuntimeError as error:
            _log_event("REVIEW_POOL_UNAVAILABLE", error=str(error), required=required)
            if reviewers_were_configured:
                raise
            selected_route = None
            break

        try:
            for observer in routes:
                if observer["instance_id"] == selected_route["instance_id"]:
                    continue
                try:
                    observed = _exchange(
                        observer["socket_path"],
                        request,
                        observer["instance_id"],
                        observe_only=True,
                    )
                    if observed.get("decision") != "observed":
                        _log_event(
                            "BROADCAST_DELIVERY_ERROR",
                            request_id=request_id,
                            instance_id=observer["instance_id"],
                            error="observer did not acknowledge broadcast",
                        )
                    else:
                        _log_event(
                            "BROADCAST_DELIVERED",
                            request_id=request_id,
                            instance_id=observer["instance_id"],
                        )
                except Exception as error:
                    _log_event(
                        "BROADCAST_DELIVERY_ERROR",
                        request_id=request_id,
                        instance_id=observer["instance_id"],
                        error=str(error),
                    )

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
                        "All registered STREENOTE review sockets are stale or unavailable; "
                        "the Hermes response was withheld."
                    ) from error
                continue
            break
        except (TimeoutError, socket.timeout) as error:
            _log_event("REVIEW_ERROR", request_id=request_id, error="socket timeout")
            raise RuntimeError(
                "STREENOTE review timed out; the Hermes response was withheld."
            ) from error
        except OSError as error:
            _log_event("REVIEW_ERROR", request_id=request_id, error=str(error))
            raise RuntimeError(
                f"STREENOTE review channel failed; the Hermes response was withheld: {error}"
            ) from error
        finally:
            _release_review_slot(slot_fd)

    if selected_route is None:
        # All terminals are connected but have deliberately disabled review.
        # They still receive the response as read-only copies; this is
        # equivalent to the existing explicit /review off override.
        for observer in routes:
            try:
                observed = _exchange(
                    observer["socket_path"],
                    request,
                    observer["instance_id"],
                    observe_only=True,
                )
                _log_event(
                    "BROADCAST_DELIVERED" if observed.get("decision") == "observed" else "BROADCAST_DELIVERY_ERROR",
                    request_id=request_id,
                    instance_id=observer["instance_id"],
                )
            except Exception as error:
                _log_event(
                    "BROADCAST_DELIVERY_ERROR",
                    request_id=request_id,
                    instance_id=observer["instance_id"],
                    error=str(error),
                )
        _broadcast_review_result(routes, request, "bypass", response_text)
        _log_event("REVIEW_BYPASS", request_id=request_id, required=required, reason="no enabled reviewers")
        return None
    if reply is None:
        raise RuntimeError(
            "No connected STREENOTE reviewer was available; the Hermes response was withheld."
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
        raise RuntimeError("STREENOTE returned an unsupported review protocol version")

    decision = reply.get("decision")
    _log_event(
        "DECISION_RECEIVED",
        request_id=request_id,
        decision=str(decision),
        instance_id=selected_route["instance_id"],
    )
    if decision == "bypass":
        # An explicit operator choice (/review off) overrides fail-closed mode.
        _broadcast_review_result(routes, request, "bypass", response_text)
        _log_event("REVIEW_BYPASS", request_id=request_id, required=required)
        return None
    if decision == "approve":
        _broadcast_review_result(routes, request, "approve", response_text)
        _log_event("REVIEW_APPROVED", request_id=request_id)
        return response_text
    if decision == "replace":
        replacement = reply.get("response_text")
        if not isinstance(replacement, str) or not replacement.strip():
            _log_event("REVIEW_ERROR", request_id=request_id, error="empty replacement")
            raise RuntimeError(
                "STREENOTE returned an empty replacement; the Hermes response was withheld."
            )
        if len(replacement.encode("utf-8")) > MAX_REVIEW_BYTES:
            _log_event("REVIEW_ERROR", request_id=request_id, error="replacement too large")
            raise RuntimeError(
                "STREENOTE replacement exceeded the review limit; the Hermes response was withheld."
            )
        _broadcast_review_result(routes, request, "replace", replacement)
        _log_event(
            "REVIEW_REPLACED",
            request_id=request_id,
            replacement_bytes=len(replacement.encode("utf-8")),
        )
        return replacement
    if decision == "reject":
        reason = reply.get("error")
        detail = f" ({reason})" if isinstance(reason, str) and reason else ""
        _broadcast_review_result(routes, request, "reject", None)
        _log_event("REVIEW_REJECTED", request_id=request_id, reason=str(reason or "")[:300])
        raise RuntimeError(f"STREENOTE operator withheld this Hermes response{detail}.")
    _log_event("REVIEW_ERROR", request_id=request_id, error=f"unknown decision {decision!r}")
    raise RuntimeError(
        f"STREENOTE returned an unknown review decision {decision!r}; the response was withheld."
    )


def register(ctx: Any) -> None:
    # Capture the user's original input inside the already-loaded plugin so
    # the first pre-LLM prompt is not dependent on a detached shell-hook process.
    ctx.register_hook("pre_llm_call", capture_user_prompt)
    # hold_stream is essential: without it, Hermes could display the original
    # draft before STREENOTE has had a chance to review or replace it.
    ctx.register_hook("transform_llm_output", review_response, hold_stream=True)
    _log_event(
        "PLUGIN_REGISTERED",
        hooks=["pre_llm_call", "transform_llm_output"],
        hold_stream=True,
    )
