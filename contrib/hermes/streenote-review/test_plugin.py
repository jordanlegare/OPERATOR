import importlib.util
import json
import os
import socket
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import patch


PLUGIN_PATH = Path(__file__).with_name("__init__.py")
SPEC = importlib.util.spec_from_file_location("streenote_review_plugin", PLUGIN_PATH)
PLUGIN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PLUGIN)

CAPTURE_PATH = PLUGIN_PATH.parent.parent / "streenote-capture.py"
CAPTURE_SPEC = importlib.util.spec_from_file_location("streenote_capture_plugin", CAPTURE_PATH)
CAPTURE = importlib.util.module_from_spec(CAPTURE_SPEC)
CAPTURE_SPEC.loader.exec_module(CAPTURE)


class HermesReviewPluginTests(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory(prefix="streenote-review-plugin-")
        self.socket_path = Path(self.temp_dir.name) / "review.sock"
        self.diagnostic_path = Path(self.temp_dir.name) / "hermes-review.log"
        self.env = patch.dict(
            os.environ,
            {
                "STREENOTE_HERMES_REVIEW_SOCKET": str(self.socket_path),
                "STREENOTE_HERMES_REVIEW_SESSION_DIR": str(Path(self.temp_dir.name) / "sessions"),
                "STREENOTE_HERMES_REVIEW_LOG": str(self.diagnostic_path),
            },
            clear=False,
        )
        self.env.start()

    def tearDown(self):
        self.env.stop()
        self.temp_dir.cleanup()

    def _serve(self, reply, socket_path=None):
        errors = []
        requests = []
        path = Path(socket_path) if socket_path is not None else self.socket_path
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(path))
        os.chmod(path, 0o600)
        listener.listen(1)
        listener.settimeout(4)

        def run():
            try:
                connection, _ = listener.accept()
                with connection:
                    data = bytearray()
                    while b"\n" not in data:
                        chunk = connection.recv(65536)
                        if not chunk:
                            raise AssertionError("plugin closed before sending request")
                        data.extend(chunk)
                    requests.append(json.loads(bytes(data).split(b"\n", 1)[0]))
                    connection.sendall(
                        json.dumps(reply, ensure_ascii=False).encode("utf-8") + b"\n"
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
                self.hook = None

            def register_hook(self, name, callback, **kwargs):
                self.hook = (name, callback, kwargs)

        context = Context()
        PLUGIN.register(context)
        self.assertIsNotNone(context.hook)
        self.assertEqual(context.hook[0], "transform_llm_output")
        self.assertIs(context.hook[1], PLUGIN.review_response)
        self.assertTrue(context.hook[2]["hold_stream"])
        records = [json.loads(line) for line in self.diagnostic_path.read_text().splitlines()]
        self.assertEqual(records[-1]["event"], "PLUGIN_REGISTERED")

    def test_pre_llm_capture_binds_each_hermes_session_to_its_originating_ssh_instance(self):
        registry = Path(os.environ["STREENOTE_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        instance_ids = ["a1", "b2", "c3"]
        workers = []
        server_error_lists = []
        for instance_id in instance_ids:
            socket_path = registry / f"{instance_id}.sock"
            worker, _, listener_errors = self._serve(
                {
                    "version": 1,
                    "decision": "replace",
                    "response_text": f"routed:{instance_id}",
                },
                socket_path=socket_path,
            )
            workers.append(worker)
            server_error_lists.append(listener_errors)
            manifest_path = registry / f"{instance_id}.json"
            manifest_path.write_text(json.dumps({
                "version": 1,
                "instance_id": instance_id,
                "socket_path": str(socket_path),
                "pid": os.getpid(),
            }))
            os.chmod(manifest_path, 0o600)

        # This is the same helper called by the existing pre_llm_call capture
        # hook in each STREENOTE-managed PTY.
        session_a = "hermes-session-origin-a"
        session_b = "hermes-session-origin-b"
        CAPTURE.write_review_session_route(registry, session_a, "a1")
        CAPTURE.write_review_session_route(registry, session_b, "c3")

        routes, directory = PLUGIN._load_review_routes()
        self.assertEqual({route["instance_id"] for route in routes}, set(instance_ids))
        self.assertEqual(
            PLUGIN._select_review_route(session_a, routes, directory)["instance_id"],
            "a1",
        )
        self.assertEqual(
            PLUGIN._select_review_route(session_b, routes, directory)["instance_id"],
            "c3",
        )

        self.assertEqual(
            PLUGIN.review_response(
                "Unreviewed response A",
                session_id=session_a,
                model="qwen3.5:latest",
                platform="api_server",
            ),
            "routed:a1",
        )
        self.assertEqual(
            PLUGIN.review_response(
                "Unreviewed response B",
                session_id=session_b,
                model="qwen3.5:latest",
                platform="api_server",
            ),
            "routed:c3",
        )

        # Existing session affinity is stable even after the active set changes.
        self.assertEqual(
            PLUGIN._select_review_route(session_a, routes, directory)["instance_id"],
            "a1",
        )
        for worker in workers:
            worker.join(timeout=5)
            self.assertFalse(worker.is_alive())
        self.assertFalse(any(server_error_lists))

    def test_multiple_review_sessions_without_origin_affinity_are_not_misrouted(self):
        registry = Path(os.environ["STREENOTE_HERMES_REVIEW_SESSION_DIR"])
        registry.mkdir(mode=0o700, parents=True)
        sockets = []
        for instance_id in ("a1", "b2"):
            socket_path = registry / f"{instance_id}.sock"
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(socket_path))
            os.chmod(socket_path, 0o600)
            listener.listen(1)
            sockets.append(listener)
            manifest_path = registry / f"{instance_id}.json"
            manifest_path.write_text(json.dumps({
                "version": 1,
                "instance_id": instance_id,
                "socket_path": str(socket_path),
            }))
            os.chmod(manifest_path, 0o600)
        try:
            with patch.dict(os.environ, {"STREENOTE_HERMES_REVIEW_REQUIRED": "1"}):
                with self.assertRaisesRegex(RuntimeError, "No STREENOTE SSH-session affinity"):
                    PLUGIN.review_response(
                        "Unreviewed response",
                        session_id="session-without-origin-affinity",
                        model="qwen3.5:latest",
                        platform="api_server",
                    )
        finally:
            for listener in sockets:
                listener.close()


    def test_runtime_socket_path_is_shared_by_processes_with_different_home_values(self):
        with tempfile.TemporaryDirectory(prefix="streenote-runtime-") as runtime:
            runtime_root = Path(runtime)
            runtime_dir = runtime_root / str(os.getuid())
            runtime_dir.mkdir()
            resolved = PLUGIN.runtime_review_socket_path(
                uid=os.getuid(), runtime_root=runtime_root
            )
            self.assertEqual(resolved, runtime_dir / "streenote" / "review.sock")

    def test_no_active_socket_leaves_ordinary_hermes_sessions_unchanged(self):
        self.assertFalse(self.socket_path.exists())
        self.assertIsNone(self._call())

    def test_required_review_fails_closed_when_socket_is_missing(self):
        with patch.dict(os.environ, {"STREENOTE_HERMES_REVIEW_REQUIRED": "1"}):
            with self.assertRaisesRegex(RuntimeError, "no active review socket.*withheld"):
                self._call()

    def test_required_review_allows_explicit_listener_bypass_when_review_is_off(self):
        worker, _, errors = self._serve({"version": 1, "decision": "bypass"})
        with patch.dict(os.environ, {"STREENOTE_HERMES_REVIEW_REQUIRED": "true"}):
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
        with patch.dict(os.environ, {"STREENOTE_HERMES_REVIEW_LOG": str(log_path)}):
            worker, _, errors = self._serve({"version": 1, "decision": "approve"})
            self.assertEqual(self._call(), "The unreviewed draft.")
            worker.join(timeout=2)
            self.assertFalse(worker.is_alive())
            self.assertFalse(errors)
        records = [json.loads(line) for line in log_path.read_text().splitlines()]
        events = [record["event"] for record in records]
        self.assertIn("CALLBACK_INVOKED", events)
        self.assertIn("SOCKET_PATH_RESOLVED", events)
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
