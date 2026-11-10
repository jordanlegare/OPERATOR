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
        })
    return routes, directory


def _route_score(session_id: str, instance_id: str) -> bytes:
    material = f"{session_id}\0{instance_id}".encode("utf-8")
    return hashlib.sha256(material).digest()


def _rendezvous_route(session_id: str, routes: list[dict[str, Any]]) -> dict[str, Any]:
    """Select a stable owner using highest-score rendezvous hashing."""
    return max(routes, key=lambda route: _route_score(session_id, route["instance_id"]))


def _write_route_assignment(path: Path, session_hash: str, instance_id: str) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        os.chmod(path.parent, 0o700)
    except OSError:
        pass
    record = {
        "version": 1,
        "session_hash": session_hash,
        "instance_id": instance_id,
    }
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(descriptor, (json.dumps(record, separators=(",", ":")) + "\n").encode("utf-8"))
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
    try:
        os.replace(temporary, path)
        os.chmod(path, 0o600)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def _select_review_route(
    session_id: str, routes: list[dict[str, Any]], registry_dir: Path
) -> dict[str, Any]:
    """Use the source PTY's captured affinity; rebalance only after that TUI exits."""
    if not session_id:
        if len(routes) == 1:
            return routes[0]
        raise RuntimeError(
            "Hermes did not provide a session_id and multiple STREENOTE review "
            "sessions are active; the response was withheld rather than routed "
            "to an arbitrary terminal."
        )

    session_hash = hashlib.sha256(session_id.encode("utf-8")).hexdigest()
    route_dir = registry_dir / "routes"
    assignment_path = route_dir / f"{session_hash}.json"
    lock_path = route_dir / f"{session_hash}.lock"
    route_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        os.chmod(route_dir, 0o700)
    except OSError:
        pass

    descriptor = os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        os.fchmod(descriptor, 0o600)
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        assigned_id = None
        try:
            metadata = assignment_path.lstat()
            if (
                stat.S_ISREG(metadata.st_mode)
                and metadata.st_uid == os.getuid()
                and not (stat.S_IMODE(metadata.st_mode) & 0o077)
                and metadata.st_size <= 4096
            ):
                assignment = json.loads(assignment_path.read_text(encoding="utf-8"))
                if assignment.get("version") == 1 and assignment.get("session_hash") == session_hash:
                    assigned_id = assignment.get("instance_id")
        except (OSError, ValueError, TypeError, json.JSONDecodeError):
            pass

        if assigned_id:
            for route in routes:
                if route["instance_id"] == assigned_id:
                    return route
            # The originating SSH session has exited. Reassign only now,
            # using rendezvous hashing across the remaining live listeners.
            selected = _rendezvous_route(session_id, routes)
            _write_route_assignment(assignment_path, session_hash, selected["instance_id"])
            return selected

        if len(routes) == 1:
            selected = routes[0]
            _write_route_assignment(assignment_path, session_hash, selected["instance_id"])
            return selected

        raise RuntimeError(
            "No STREENOTE SSH-session affinity was captured for this Hermes session. "
            "The capture hook must run in the originating STREENOTE PTY; refusing to "
            "route the answer to another SSH terminal."
        )
    finally:
        fcntl.flock(descriptor, fcntl.LOCK_UN)
        os.close(descriptor)


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


def _exchange(path: Path, request: dict[str, Any], instance_id: str) -> dict[str, Any]:
    encoded = json.dumps(
        request, ensure_ascii=False, separators=(",", ":")
    ).encode("utf-8")
    if len(encoded) > MAX_REVIEW_BYTES:
        raise RuntimeError("Hermes final response exceeds STREENOTE's 8 MiB review limit")

    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.settimeout(SOCKET_TIMEOUT_SECONDS)
    try:
        try:
            connection.connect(str(path))
        except (FileNotFoundError, ConnectionRefusedError) as error:
            raise _ReviewSocketUnavailable(str(error)) from error
        _log_event(
            "SOCKET_CONNECT_OK",
            socket_path=str(path),
            instance_id=instance_id,
            request_id=request.get("request_id", ""),
        )
        with connection:
            connection.sendall(encoded + b"\n")
            _log_event("REQUEST_SENT", request_id=request.get("request_id", ""), instance_id=instance_id)
            return _read_json_line(connection)
    except Exception:
        connection.close()
        raise


def review_response(
    response_text: str,
    session_id: str = "",
    model: str = "",
    platform: str = "",
    **kwargs: Any,
) -> str | None:
    """Hold the final reply at the STREENOTE instance assigned to this Hermes session."""
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

    try:
        route = _select_review_route(str(session_id or ""), routes, registry_dir)
    except RuntimeError as error:
        # Multiple live SSH listeners with no trustworthy affinity is never
        # a pass-through condition. Returning None here silently releases an
        # unreviewed answer, which defeats /review on in every active TUI.
        _log_event(
            "REVIEW_ROUTE_ERROR",
            error=str(error),
            required=required,
            fail_closed=True,
        )
        raise RuntimeError(
            f"STREENOTE could not resolve the originating SSH review session; "
            f"the Hermes response was withheld: {error}"
        ) from error

    session_hash = hashlib.sha256(str(session_id or "").encode("utf-8")).hexdigest()[:16]
    _log_event(
        "SESSION_ROUTE_SELECTED",
        session_hash=session_hash,
        instance_id=route["instance_id"],
        socket_path=str(route["socket_path"]),
    )

    request_id = uuid.uuid4().hex
    request = {
        "version": 1,
        "request_id": request_id,
        "session_id": str(session_id or "")[:MAX_METADATA_CHARS],
        "model": str(model or "")[:MAX_METADATA_CHARS],
        "platform": str(platform or "")[:128],
        "response_text": response_text,
    }

    # Retry only when connect() proves the selected process is already gone.
    # Once connected, never send the same request to another TUI: the first
    # listener may already have displayed it to its operator.
    remaining = list(routes)
    reply = None
    selected_route = route
    while remaining:
        try:
            reply = _exchange(
                selected_route["socket_path"],
                request,
                selected_route["instance_id"],
            )
            route = selected_route
            break
        except _ReviewSocketUnavailable as error:
            _log_event(
                "SOCKET_CONNECT_ERROR",
                socket_path=str(selected_route["socket_path"]),
                instance_id=selected_route["instance_id"],
                error=str(error),
                request_id=request_id,
            )
            manifest = selected_route.get("manifest_path")
            if manifest is not None:
                try:
                    manifest.unlink()
                except OSError:
                    pass
            # A refused connection proves the same-UID legacy socket file is
            # stale too; remove it so future calls do not keep selecting it.
            try:
                selected_route["socket_path"].unlink()
            except OSError:
                pass
            remaining = [
                candidate for candidate in remaining
                if candidate["instance_id"] != selected_route["instance_id"]
            ]
            if not remaining:
                raise RuntimeError(
                    "All registered STREENOTE review sockets are stale or unavailable; "
                    "the Hermes response was withheld."
                ) from error
            selected_route = _select_review_route(
                str(session_id or ""), remaining, registry_dir
            )
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
        except RuntimeError as error:
            _log_event("REVIEW_ERROR", request_id=request_id, error=str(error))
            raise

    if reply is None:
        raise RuntimeError("STREENOTE did not return a review decision; the response was withheld.")

    if reply.get("version") != 1:
        _log_event("REVIEW_ERROR", request_id=request_id, error="unsupported protocol version")
        raise RuntimeError("STREENOTE returned an unsupported review protocol version")

    decision = reply.get("decision")
    _log_event(
        "DECISION_RECEIVED",
        request_id=request_id,
        decision=str(decision),
        instance_id=route["instance_id"],
    )
    if decision == "bypass":
        # An explicit operator choice (/review off) overrides fail-closed mode.
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
                "STREENOTE returned an empty replacement; the Hermes response was withheld."
            )
        if len(replacement.encode("utf-8")) > MAX_REVIEW_BYTES:
            _log_event("REVIEW_ERROR", request_id=request_id, error="replacement too large")
            raise RuntimeError(
                "STREENOTE replacement exceeded the review limit; the Hermes response was withheld."
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
        raise RuntimeError(f"STREENOTE operator withheld this Hermes response{detail}.")
    _log_event("REVIEW_ERROR", request_id=request_id, error=f"unknown decision {decision!r}")
    raise RuntimeError(
        f"STREENOTE returned an unknown review decision {decision!r}; the response was withheld."
    )


def register(ctx: Any) -> None:
    # hold_stream is essential: without it, Hermes could display the original
    # draft before STREENOTE has had a chance to review or replace it.
    ctx.register_hook("transform_llm_output", review_response, hold_stream=True)
    _log_event("PLUGIN_REGISTERED", hook="transform_llm_output", hold_stream=True)
