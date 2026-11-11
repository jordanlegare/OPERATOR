import hashlib
import importlib.util
import json
import os
import socket
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch


PLUGIN_PATH = Path(__file__).with_name("__init__.py")
SPEC = importlib.util.spec_from_file_location("operator_review_plugin", PLUGIN_PATH)
PLUGIN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PLUGIN)

CAPTURE_PATH = PLUGIN_PATH.parent.parent / "hooks" / "operator-capture.py"
CAPTURE_SPEC = importlib.util.spec_from_file_location("operator_capture_plugin", CAPTURE_PATH)
CAPTURE = importlib.util.module_from_spec(CAPTURE_SPEC)
CAPTURE_SPEC.loader.exec_module(CAPTURE)


class HermesReviewPluginTests(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory(prefix="operator-review-plugin-")
        self.socket_path = Path(self.temp_dir.name) / "review.sock"
        self.diagnostic_path = Path(self.temp_dir.name) / "hermes-review.log"
        self.env = patch.dict(
            os.environ,
            {
                "OPERATOR_HERMES_REVIEW_SOCKET": str(self.socket_path),
                "OPERATOR_HERMES_REVIEW_SESSION_DIR": str(Path(self.temp_dir.name) / "sessions"),
                "OPERATOR_HERMES_REVIEW_LOG": str(self.diagnostic_path),
            },
            clear=False,
        )
        self.env.start()

    def tearDown(self):
        self.env.stop()
        self.temp_dir.cleanup()

    def test_legacy_environment_alias_remains_supported(self):
        with patch.dict(os.environ, {"STREENOTE_HERMES_REVIEW_REQUIRED": "1"}, clear=True):
            self.assertEqual(PLUGIN.env_value("OPERATOR_HERMES_REVIEW_REQUIRED"), "1")

    def test_operator_environment_name_takes_precedence(self):
        with patch.dict(os.environ, {
            "STREENOTE_HERMES_REVIEW_REQUIRED": "0",
            "OPERATOR_HERMES_REVIEW_REQUIRED": "1",
        }, clear=True):
            self.assertEqual(PLUGIN.env_value("OPERATOR_HERMES_REVIEW_REQUIRED"), "1")

    def _serve(self, reply, socket_path=None, accept_count=1):
        errors = []
        requests = []
        path = Path(socket_path) if socket_path is not None else self.socket_path
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(path))
        os.chmod(path, 0o600)
        listener.listen(accept_count)
        listener.settimeout(4)

        def run():
            try:
                for _ in range(accept_count):
                    connection, _ = listener.accept()
                    with connection:
                        data = bytearray()
                        while b"\n" not in data:
                            chunk = connection.recv(65536)
                            if not chunk:
                                raise AssertionError("plugin closed before sending request")
                            data.extend(chunk)
                        request = json.loads(bytes(data).split(b"\n", 1)[0])
                        requests.append(request)
                        response = (
                            {"version": 1, "decision": "observed"}
                            if request.get("observe_only")
                            else reply
                        )
                        connection.sendall(
                            json.dumps(response, ensure_ascii=False).encode("utf-8") + b"\n"
                        )
            except socket.timeout:
                pass
            except Exception as error:  # surfaced in the test thread below
                errors.append(error)
            finally:
                listener.close()
                try:
                    path.unlink()
                except FileNotFoundError:
                    pass

        worker = threading.Thread(target=run, daemon=True)
        worker.start()
        return worker, requests, errors
    def _call(self, session_id="session-123"):
        return PLUGIN.review_response(
            "The unreviewed draft.",
            session_id=session_id,
            model="qwen3.5:latest",
            platform="tui",
        )

    def test_register_installs_held_transform_and_logs_registration(self):
        class Context:
            def __init__(self):
                self.hooks = {}

            def register_hook(self, name, callback, **kwargs):
                self.hooks[name] = (callback, kwargs)

        context = Context()
        PLUGIN.register(context)
        self.assertEqual(
            set(context.hooks),
            {"pre_llm_call", "pre_api_request", "post_llm_call", "transform_llm_output"},
        )
        self.assertIs(context.hooks["pre_llm_call"][0], PLUGIN.capture_pre_llm_prompt)
        self.assertIs(context.hooks["pre_api_request"][0], PLUGIN.capture_pre_api_prompt)
        self.assertIs(context.hooks["post_llm_call"][0], PLUGIN.capture_post_llm_prompt)
        self.assertEqual(context.hooks["pre_llm_call"][1], {})
        self.assertIs(context.hooks["transform_llm_output"][0], PLUGIN.review_response)
        self.assertTrue(context.hooks["transform_llm_output"][1]["hold_stream"])
        records = [json.loads(line) for line in self.diagnostic_path.read_text().splitlines()]
        self.assertEqual(records[-1]["event"], "PLUGIN_REGISTERED")

    def test_response_is_dispatched_only_to_enabled_reviewer_terminals(self):
        registry = Path(os.environ["OPERATOR_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        instance_ids = ("a1", "b2", "c3")
        workers = []
        requests_by_instance = {}
        errors_by_instance = {}

        for instance_id in instance_ids:
            socket_path = registry / f"{instance_id}.sock"
            worker, requests, errors = self._serve(
                {"version": 1, "decision": "approve"},
                socket_path=socket_path,
                accept_count=1,
            )
            workers.append(worker)
            requests_by_instance[instance_id] = requests
            errors_by_instance[instance_id] = errors
            manifest = registry / f"{instance_id}.json"
            manifest.write_text(json.dumps({
                "version": 1,
                "instance_id": instance_id,
                "socket_path": str(socket_path),
                "review_enabled": instance_id != "c3",
            }))
            os.chmod(manifest, 0o600)

        result = PLUGIN.review_response(
            "One response should be delivered to one available reviewer.",
            session_id="shared-session-1",
            model="qwen3.5:latest",
            platform="api_server",
        )
        self.assertEqual(
            result,
            "One response should be delivered to one available reviewer.",
        )

        for worker in workers:
            worker.join(timeout=5)
            self.assertFalse(worker.is_alive())

        self.assertFalse(any(error for errors in errors_by_instance.values() for error in errors))
        delivered = {
            instance_id: requests[0]
            for instance_id, requests in requests_by_instance.items()
            if requests
        }
        self.assertEqual(len(delivered), 1)
        reviewer_id, request = next(iter(delivered.items()))
        self.assertIn(reviewer_id, ("a1", "b2"))
        self.assertFalse(request.get("observe_only", False))
        self.assertEqual(request["response_text"], "One response should be delivered to one available reviewer.")
        self.assertNotIn("c3", delivered)

    def test_edited_final_answer_is_returned_only_by_the_selected_reviewer(self):
        registry = Path(os.environ["OPERATOR_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        requests_by_instance = {}
        workers = []
        errors_by_instance = {}
        for instance_id, review_enabled in (("a1", True), ("b2", False)):
            socket_path = registry / f"{instance_id}.sock"
            worker, requests, errors = self._serve(
                {
                    "version": 1,
                    "decision": "replace",
                    "response_text": "Edited answer returned only to Hermes.",
                },
                socket_path=socket_path,
                accept_count=1,
            )
            workers.append(worker)
            requests_by_instance[instance_id] = requests
            errors_by_instance[instance_id] = errors
            manifest = registry / f"{instance_id}.json"
            manifest.write_text(json.dumps({
                "version": 1,
                "instance_id": instance_id,
                "socket_path": str(socket_path),
                "review_enabled": review_enabled,
            }))
            os.chmod(manifest, 0o600)

        result = PLUGIN.review_response(
            "Original unedited answer.",
            session_id="edited-review-session",
            model="qwen3.5:latest",
            platform="api_server",
        )
        self.assertEqual(result, "Edited answer returned only to Hermes.")

        for worker in workers:
            worker.join(timeout=5)
            self.assertFalse(worker.is_alive())
        self.assertFalse(any(error for errors in errors_by_instance.values() for error in errors))
        self.assertEqual(len(requests_by_instance["a1"]), 1)
        self.assertFalse(requests_by_instance["a1"][0].get("observe_only", False))
        self.assertEqual(requests_by_instance["b2"], [])

    def test_all_reviewers_opted_out_bypass_without_contact_even_when_review_required(self):
        registry = Path(os.environ["OPERATOR_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        socket_path = registry / "a1.sock"
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(socket_path))
        os.chmod(socket_path, 0o600)
        listener.listen(1)
        listener.settimeout(0.15)
        manifest = registry / "a1.json"
        manifest.write_text(json.dumps({
            "version": 1,
            "instance_id": "a1",
            "socket_path": str(socket_path),
            "review_enabled": False,
        }))
        os.chmod(manifest, 0o600)

        with patch.dict(os.environ, {"OPERATOR_HERMES_REVIEW_REQUIRED": "1"}):
            self.assertIsNone(self._call())
        with self.assertRaises(socket.timeout):
            listener.accept()
        listener.close()

    def test_scheduler_adds_rejoined_operator_while_another_reviewer_is_busy(self):
        registry = Path(os.environ["OPERATOR_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        sockets = {}
        manifests = {}
        for instance_id, enabled in (("a1", True), ("b2", False)):
            socket_path = registry / f"{instance_id}.sock"
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(socket_path))
            os.chmod(socket_path, 0o600)
            listener.listen(1)
            sockets[instance_id] = listener
            manifest = registry / f"{instance_id}.json"
            manifest.write_text(json.dumps({
                "version": 1,
                "instance_id": instance_id,
                "socket_path": str(socket_path),
                "review_enabled": enabled,
            }))
            os.chmod(manifest, 0o600)
            manifests[instance_id] = manifest

        routes, registry_dir = PLUGIN._load_review_routes()
        lock_path = registry / "dispatch" / "a1.lock"
        lock_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o600)
        PLUGIN.fcntl.flock(lock_fd, PLUGIN.fcntl.LOCK_EX | PLUGIN.fcntl.LOCK_NB)
        outcome = []
        worker = threading.Thread(
            target=lambda: outcome.append(
                PLUGIN._acquire_review_slot(routes, registry_dir, timeout=1.5)
            ),
            daemon=True,
        )
        worker.start()
        selected_fd = None
        try:
            time.sleep(0.15)
            temporary = manifests["b2"].with_suffix(".json.tmp")
            temporary.write_text(json.dumps({
                "version": 1,
                "instance_id": "b2",
                "socket_path": str(sockets["b2"].getsockname() or registry / "b2.sock"),
                "review_enabled": True,
            }))
            os.chmod(temporary, 0o600)
            os.replace(temporary, manifests["b2"])
            worker.join(timeout=1)
            self.assertFalse(worker.is_alive(), "dispatcher should notice /review on promptly")
            self.assertIsNotNone(outcome[0])
            selected_route, selected_fd = outcome[0]
            self.assertEqual(selected_route["instance_id"], "b2")
        finally:
            if selected_fd is not None:
                PLUGIN._release_review_slot(selected_fd)
            PLUGIN.fcntl.flock(lock_fd, PLUGIN.fcntl.LOCK_UN)
            os.close(lock_fd)
            for listener in sockets.values():
                listener.close()

    def test_scheduler_refreshes_live_registration_when_operator_opts_out_while_queued(self):
        registry = Path(os.environ["OPERATOR_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        socket_path = registry / "a1.sock"
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(socket_path))
        os.chmod(socket_path, 0o600)
        listener.listen(1)
        manifest = registry / "a1.json"
        manifest.write_text(json.dumps({
            "version": 1,
            "instance_id": "a1",
            "socket_path": str(socket_path),
            "review_enabled": True,
        }))
        os.chmod(manifest, 0o600)

        routes, registry_dir = PLUGIN._load_review_routes()
        lock_path = registry / "dispatch" / "a1.lock"
        lock_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o600)
        PLUGIN.fcntl.flock(lock_fd, PLUGIN.fcntl.LOCK_EX | PLUGIN.fcntl.LOCK_NB)
        outcome = []
        worker = threading.Thread(
            target=lambda: outcome.append(
                PLUGIN._acquire_review_slot(routes, registry_dir, timeout=1.5)
            ),
            daemon=True,
        )
        worker.start()
        try:
            time.sleep(0.15)
            temporary = manifest.with_suffix(".json.tmp")
            temporary.write_text(json.dumps({
                "version": 1,
                "instance_id": "a1",
                "socket_path": str(socket_path),
                "review_enabled": False,
            }))
            os.chmod(temporary, 0o600)
            os.replace(temporary, manifest)
            worker.join(timeout=1)
            self.assertFalse(worker.is_alive(), "dispatcher should notice /review off promptly")
            self.assertEqual(outcome, [None])
        finally:
            PLUGIN.fcntl.flock(lock_fd, PLUGIN.fcntl.LOCK_UN)
            os.close(lock_fd)
            listener.close()

    def test_review_pool_round_robins_and_skips_a_terminal_with_an_active_lease(self):
        registry = Path(os.environ["OPERATOR_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        routes = [
            {"instance_id": name, "review_enabled": True}
            for name in ("a1", "b2", "c3")
        ]

        first, first_fd = PLUGIN._acquire_review_slot(routes, registry, timeout=0.2)
        second, second_fd = PLUGIN._acquire_review_slot(routes, registry, timeout=0.2)
        third, third_fd = PLUGIN._acquire_review_slot(routes, registry, timeout=0.2)
        try:
            self.assertEqual(
                {first["instance_id"], second["instance_id"], third["instance_id"]},
                {"a1", "b2", "c3"},
            )
            fourth, fourth_fd = None, None
            PLUGIN._release_review_slot(second_fd)
            second_fd = -1
            fourth, fourth_fd = PLUGIN._acquire_review_slot(routes, registry, timeout=0.2)
            self.assertEqual(fourth["instance_id"], "b2")
            PLUGIN._release_review_slot(fourth_fd)
            fourth_fd = -1
        finally:
            PLUGIN._release_review_slot(first_fd)
            PLUGIN._release_review_slot(third_fd)
            if second_fd >= 0:
                PLUGIN._release_review_slot(second_fd)


    def test_pre_llm_plugin_hook_fans_first_prompt_to_every_live_terminal_inbox(self):
        registry = Path(os.environ["OPERATOR_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        listeners = []
        instance_ids = ("a1", "b2", "c3")
        for instance_id in instance_ids:
            socket_path = registry / f"{instance_id}.sock"
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(socket_path))
            os.chmod(socket_path, 0o600)
            listener.listen(1)
            listeners.append(listener)
            manifest = registry / f"{instance_id}.json"
            manifest.write_text(json.dumps({
                "version": 1,
                "instance_id": instance_id,
                "socket_path": str(socket_path),
                "review_enabled": True,
            }))
            os.chmod(manifest, 0o600)

        PLUGIN.capture_user_prompt(
            session_id="first-prompt-session",
            turn_id="first-turn-001",
            user_message="Inspect the gateway; api_key=do-not-store-this",
            platform="cli",
        )

        expected_event_id = CAPTURE.make_event_id(
            "pre_llm_call", "first-prompt-session", "first-turn-001"
        )
        for instance_id in instance_ids:
            inbox = registry / "inboxes" / instance_id / "ready"
            records = list(inbox.glob("*.json"))
            self.assertEqual(len(records), 1, f"{instance_id} must receive the first prompt")
            event = json.loads(records[0].read_text(encoding="utf-8"))
            self.assertEqual(event["kind"], "hermes_user")
            self.assertEqual(event["event_id"], expected_event_id)
            self.assertEqual(
                event["text"],
                "Inspect the gateway; api_key=[REDACTED]",
            )

        records = [json.loads(line) for line in self.diagnostic_path.read_text().splitlines()]
        prompt_events = [record for record in records if record["event"] == "PROMPT_FANOUT"]
        self.assertEqual(len(prompt_events), 1)
        self.assertEqual(prompt_events[0]["delivered"], 3)
        self.assertEqual(prompt_events[0]["recipients"], 3)

        for listener in listeners:
            listener.close()


    def test_prompt_capture_falls_back_to_last_user_message_in_conversation_history(self):
        registry = Path(os.environ["OPERATOR_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        listeners = []
        for instance_id in ("a1", "b2"):
            socket_path = registry / f"{instance_id}.sock"
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(socket_path))
            os.chmod(socket_path, 0o600)
            listener.listen(1)
            listeners.append(listener)
            manifest = registry / f"{instance_id}.json"
            manifest.write_text(json.dumps({
                "version": 1,
                "instance_id": instance_id,
                "socket_path": str(socket_path),
                "review_enabled": True,
            }))
            os.chmod(manifest, 0o600)

        PLUGIN.capture_pre_api_prompt(
            session_id="fallback-session",
            turn_id="fallback-turn",
            user_message=None,
            conversation_history=[
                {"role": "system", "content": "not captured"},
                {"role": "user", "content": "The first prompt is in history."},
            ],
        )

        for instance_id in ("a1", "b2"):
            inbox = registry / "inboxes" / instance_id / "ready"
            events = [json.loads(path.read_text()) for path in inbox.glob("*.json")]
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["text"], "The first prompt is in history.")
            self.assertEqual(
                events[0]["event_id"],
                CAPTURE.make_event_id(
                    "pre_llm_call", "fallback-session", "fallback-turn"
                ),
            )

        records = [json.loads(line) for line in self.diagnostic_path.read_text().splitlines()]
        invoked = [record for record in records if record["event"] == "PROMPT_HOOK_INVOKED"]
        self.assertEqual(len(invoked), 1)
        self.assertEqual(invoked[0]["hook"], "pre_api_request")
        self.assertEqual(invoked[0]["message_source"], "conversation_history")

        for listener in listeners:
            listener.close()

    def test_capture_fans_each_hook_event_out_to_each_live_terminal_inbox(self):
        registry = Path(os.environ["OPERATOR_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        listeners = []
        instance_ids = ("a1", "b2", "c3")
        for instance_id in instance_ids:
            socket_path = registry / f"{instance_id}.sock"
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(socket_path))
            os.chmod(socket_path, 0o600)
            listener.listen(1)
            listeners.append(listener)
            manifest = registry / f"{instance_id}.json"
            manifest.write_text(json.dumps({
                "version": 1,
                "instance_id": instance_id,
                "socket_path": str(socket_path),
                "review_enabled": True,
            }))
            os.chmod(manifest, 0o600)

        event = {
            "version": 1,
            "event_id": "fanout-event-001",
            "kind": "hermes_user",
            "timestamp": "2026-11-10T12:00:00-05:00",
            "text": "A prompt that every connected terminal must receive.",
            "session_id": "session-shared",
        }
        CAPTURE.write_inbox_event(Path(self.temp_dir.name) / "unused-single-inbox", event)

        for instance_id in instance_ids:
            inbox = registry / "inboxes" / instance_id / "ready"
            records = list(inbox.glob("*.json"))
            self.assertEqual(len(records), 1, f"{instance_id} must get its own event copy")
            self.assertEqual(json.loads(records[0].read_text())["event_id"], "fanout-event-001")

        for listener in listeners:
            listener.close()

    def test_runtime_socket_path_is_shared_by_processes_with_different_home_values(self):
        with tempfile.TemporaryDirectory(prefix="operator-runtime-") as runtime:
            runtime_root = Path(runtime)
            runtime_dir = runtime_root / str(os.getuid())
            runtime_dir.mkdir()
            resolved = PLUGIN.runtime_review_socket_path(
                uid=os.getuid(), runtime_root=runtime_root
            )
            self.assertEqual(resolved, runtime_dir / "operator" / "review.sock")

    def test_no_active_socket_leaves_ordinary_hermes_sessions_unchanged(self):
        self.assertFalse(self.socket_path.exists())
        self.assertIsNone(self._call())

    def test_required_review_fails_closed_when_socket_is_missing(self):
        with patch.dict(os.environ, {"OPERATOR_HERMES_REVIEW_REQUIRED": "1"}):
            with self.assertRaisesRegex(RuntimeError, "no active review socket.*withheld"):
                self._call()

    def test_required_review_allows_explicit_listener_bypass_when_review_is_off(self):
        worker, _, errors = self._serve({"version": 1, "decision": "bypass"})
        with patch.dict(os.environ, {"OPERATOR_HERMES_REVIEW_REQUIRED": "true"}):
            self.assertIsNone(self._call())
        worker.join(timeout=2)
        self.assertFalse(worker.is_alive())
        self.assertFalse(errors)
        records = [json.loads(line) for line in self.diagnostic_path.read_text().splitlines()]
        bypass = [record for record in records if record["event"] == "REVIEW_BYPASS"]
        self.assertEqual(len(bypass), 1)
        self.assertTrue(bypass[0]["required"])

    def test_callback_and_socket_decisions_are_logged_without_answer_content(self):
        log_path = Path(self.temp_dir.name) / "diagnostics.jsonl"
        with patch.dict(os.environ, {"OPERATOR_HERMES_REVIEW_LOG": str(log_path)}):
            worker, _, errors = self._serve({"version": 1, "decision": "approve"})
            self.assertEqual(self._call(), "The unreviewed draft.")
            worker.join(timeout=2)
            self.assertFalse(worker.is_alive())
            self.assertFalse(errors)
        records = [json.loads(line) for line in log_path.read_text().splitlines()]
        events = [record["event"] for record in records]
        self.assertIn("CALLBACK_INVOKED", events)
        self.assertIn("SOCKET_ROUTES_RESOLVED", events)
        self.assertIn("SESSION_REVIEW_ASSIGNED", events)
        self.assertIn("SOCKET_CONNECT_OK", events)
        self.assertIn("REQUEST_SENT", events)
        self.assertIn("DECISION_RECEIVED", events)
        self.assertIn("REVIEW_APPROVED", events)
        self.assertNotIn("The unreviewed draft.", log_path.read_text())

    def test_bypass_and_approve_leave_the_original_answer_unchanged(self):
        for decision in ("bypass", "approve"):
            with self.subTest(decision=decision):
                worker, requests, errors = self._serve(
                    {"version": 1, "decision": decision}
                )
                result = self._call()
                if decision == "approve":
                    self.assertEqual(result, "The unreviewed draft.")
                else:
                    self.assertIsNone(result)
                worker.join(timeout=2)
                self.assertFalse(worker.is_alive())
                self.assertFalse(errors)
                self.assertEqual(requests[0]["response_text"], "The unreviewed draft.")
                self.assertEqual(requests[0]["session_id"], "session-123")
                self.assertEqual(requests[0]["model"], "qwen3.5:latest")
                self.assertEqual(requests[0]["platform"], "tui")

    def test_replace_returns_the_operator_edited_answer_to_hermes(self):
        worker, requests, errors = self._serve(
            {
                "version": 1,
                "decision": "replace",
                "response_text": "The edited and approved answer.",
            }
        )
        result = self._call()
        worker.join(timeout=2)
        self.assertFalse(worker.is_alive())
        self.assertFalse(errors)
        self.assertEqual(result, "The edited and approved answer.")
        self.assertEqual(len(requests), 1)
        self.assertEqual(requests[0]["version"], 1)
        self.assertTrue(requests[0]["request_id"])

    def test_rejection_withholds_the_original_answer(self):
        worker, _, errors = self._serve(
            {
                "version": 1,
                "decision": "reject",
                "error": "operator rejected this draft",
            }
        )
        with self.assertRaisesRegex(RuntimeError, "withheld"):
            self._call()
        worker.join(timeout=2)
        self.assertFalse(worker.is_alive())
        self.assertFalse(errors)

    def test_invalid_empty_replacement_is_fail_closed(self):
        worker, _, errors = self._serve(
            {"version": 1, "decision": "replace", "response_text": "   "}
        )
        with self.assertRaisesRegex(RuntimeError, "empty replacement"):
            self._call()
        worker.join(timeout=2)
        self.assertFalse(worker.is_alive())
        self.assertFalse(errors)

    def test_stale_socket_path_is_not_treated_as_inactive_review(self):
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(self.socket_path))
        os.chmod(self.socket_path, 0o600)
        listener.close()
        with self.assertRaisesRegex(RuntimeError, "stale"):
            self._call()


if __name__ == "__main__":
    unittest.main()
