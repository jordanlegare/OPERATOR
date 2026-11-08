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


class HermesReviewPluginTests(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory(prefix="streenote-review-plugin-")
        self.socket_path = Path(self.temp_dir.name) / "review.sock"
        self.env = patch.dict(
            os.environ,
            {"STREENOTE_HERMES_REVIEW_SOCKET": str(self.socket_path)},
            clear=False,
        )
        self.env.start()

    def tearDown(self):
        self.env.stop()
        self.temp_dir.cleanup()

    def _serve(self, reply):
        errors = []
        requests = []
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(self.socket_path))
        listener.listen(1)

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
            except Exception as error:  # surfaced in the test thread below
                errors.append(error)
            finally:
                listener.close()
                try:
                    self.socket_path.unlink()
                except FileNotFoundError:
                    pass

        worker = threading.Thread(target=run, daemon=True)
        worker.start()
        return worker, requests, errors

    def _call(self):
        return PLUGIN.review_response(
            "The unreviewed draft.",
            session_id="session-123",
            model="qwen3.5:latest",
            platform="tui",
        )

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
        listener.close()
        with self.assertRaisesRegex(RuntimeError, "stale"):
            self._call()


if __name__ == "__main__":
    unittest.main()
