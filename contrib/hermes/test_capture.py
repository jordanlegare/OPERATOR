"""Regression tests for original Hermes prompt capture and terminal fan-out."""

from __future__ import annotations

import importlib.util
import io
import json
import os
import socket
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("streenote-capture.py")
SPEC = importlib.util.spec_from_file_location("streenote_capture", SCRIPT)
CAPTURE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(CAPTURE)


class CapturePromptFanoutTests(unittest.TestCase):
    def setUp(self):
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name)
        self.registry = self.root / "review-sessions"
        self.registry.mkdir(mode=0o700)
        self.inboxes = {
            "a1": self.registry / "inboxes" / "a1",
            "b2": self.registry / "inboxes" / "b2",
        }
        self.sockets = []
        for instance_id in self.inboxes:
            socket_path = self.registry / f"{instance_id}.sock"
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(socket_path))
            os.chmod(socket_path, 0o600)
            self.sockets.append(listener)

            manifest = self.registry / f"{instance_id}.json"
            manifest.write_text(json.dumps({
                "version": 1,
                "instance_id": instance_id,
                "socket_path": str(socket_path),
                "review_enabled": True,
            }), encoding="utf-8")
            os.chmod(manifest, 0o600)

    def tearDown(self):
        for listener in self.sockets:
            listener.close()
        self.tempdir.cleanup()

    def _run_hook(self, payload, *, include_inbox_env=True):
        environment = {}
        if include_inbox_env:
            environment["STREENOTE_HERMES_INBOX"] = str(self.inboxes["a1"])

        with patch.dict(CAPTURE.os.environ, environment, clear=True), patch.object(
            CAPTURE,
            "review_session_registry_candidates",
            return_value=[
                self.root / "stale-review-registry",
                self.registry,
            ],
        ):
            previous_stdin = CAPTURE.sys.stdin
            CAPTURE.sys.stdin = io.StringIO(json.dumps(payload))
            try:
                expected_inbox = environment.get("STREENOTE_HERMES_INBOX")
                self.assertEqual(
                    CAPTURE.os.environ.get("STREENOTE_HERMES_INBOX"),
                    expected_inbox,
                )
                self.assertEqual(CAPTURE.main(), 0)
            finally:
                CAPTURE.sys.stdin = previous_stdin

    def _events(self, inbox):
        ready = inbox / "ready"
        return [
            json.loads(path.read_text(encoding="utf-8"))
            for path in sorted(ready.glob("*.json"))
        ]

    def test_original_prompt_and_initial_history_reach_every_terminal(self):
        # Simulate a systemd gateway that did not inherit the
        # STREENOTE_HERMES_REVIEW_SESSION_DIR from an SSH PTY.
        self._run_hook({
            "hook_event_name": "pre_llm_call",
            "session_id": "shared-hermes-session",
            "extra": {
                "turn_id": "turn-001",
                "user_message": "Inspect the deployment and explain the rollback plan.",
                "conversation_history": [
                    {"role": "system", "content": "Do not capture this system instruction."},
                    {"role": "user", "content": "Earlier user question."},
                    {"role": "assistant", "content": "Earlier assistant answer."},
                    {"role": "tool", "name": "terminal", "content": "api_key=super-secret"},
                ],
            },
        }, include_inbox_env=False)

        copies = [self._events(inbox) for inbox in self.inboxes.values()]
        self.assertEqual(len(copies), 2)
        for events in copies:
            by_kind = {event["kind"]: event for event in events}
            self.assertEqual(set(by_kind), {"hermes_history", "hermes_user"})
            prompt = by_kind["hermes_user"]
            history = by_kind["hermes_history"]
            self.assertEqual(
                prompt["text"],
                "Inspect the deployment and explain the rollback plan.",
            )
            self.assertEqual(prompt["session_id"], "shared-hermes-session")
            self.assertEqual(prompt["turn_id"], "turn-001")
            self.assertIn("Earlier user question.", history["text"])
            self.assertIn("Earlier assistant answer.", history["text"])
            self.assertIn("api_key=[REDACTED]", history["text"])
            self.assertNotIn("Do not capture this system instruction.", history["text"])

        self.assertEqual(
            copies[0][0]["event_id"],
            copies[1][0]["event_id"],
            "every terminal must receive the identical history event",
        )
        self.assertEqual(
            copies[0][1]["event_id"],
            copies[1][1]["event_id"],
            "every terminal must receive the identical original prompt event",
        )

    def test_multimodal_prompt_keeps_text_and_never_persists_binary_payload(self):
        secret_image_payload = "data:image/png;base64,THIS_MUST_NOT_BE_STORED"
        self._run_hook({
            "hook_event_name": "pre_llm_call",
            "session_id": "multimodal-session",
            "extra": {
                "turn_id": "turn-image",
                "user_message": [
                    {"type": "text", "text": "Describe the attached diagram."},
                    {"type": "image_url", "image_url": {"url": secret_image_payload}},
                ],
            },
        })

        for inbox in self.inboxes.values():
            events = self._events(inbox)
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["kind"], "hermes_user")
            self.assertEqual(
                events[0]["text"],
                "Describe the attached diagram.\n[image attachment]",
            )
            self.assertNotIn(secret_image_payload, events[0]["text"])


if __name__ == "__main__":
    unittest.main()
