# Copyright AGNTCY Contributors (https://github.com/agntcy)
# Copyright A2A Contributors (https://github.com/a2aproject)
# SPDX-License-Identifier: Apache-2.0
"""Negative controls for the live interop assertions (no external services)."""

import json
import subprocess
import unittest

from slimrpc_interop import assert_echo, check_response


class EchoAssertions(unittest.TestCase):
    message = "unique echo text"

    def task(self):
        return {"status": {"state": "completed"},
                "artifacts": [{"parts": [{"text": self.message}]}]}

    def stream(self):
        return [{"artifactUpdate": {"artifact": self.task()["artifacts"][0]}},
                {"statusUpdate": {"status": {"state": "TASK_STATE_COMPLETED"}}}]

    def test_unary_and_streaming(self):
        assert_echo(json.dumps({"task": self.task()}), self.message)
        assert_echo("\n".join(map(json.dumps, self.stream())), self.message, True)

    def test_request_history_is_not_an_echo(self):
        task = self.task()
        task.pop("artifacts")
        task["history"] = [{"parts": [{"text": self.message}]}]
        with self.assertRaisesRegex(ValueError, "artifact"):
            assert_echo(json.dumps({"task": task}), self.message)

    def test_wrong_artifact(self):
        with self.assertRaisesRegex(ValueError, "artifact"):
            assert_echo(json.dumps(self.task()), "different request")

    def test_incomplete_task(self):
        task = self.task()
        task["status"]["state"] = "working"
        with self.assertRaisesRegex(ValueError, "completed"):
            assert_echo(json.dumps(task), self.message)

    def test_truncated_stream(self):
        with self.assertRaisesRegex(ValueError, "completed"):
            assert_echo(json.dumps(self.stream()[0]), self.message, True)

    def test_nonfinal_stream(self):
        events = self.stream()
        events[-1]["statusUpdate"]["final"] = False
        with self.assertRaisesRegex(ValueError, "not final"):
            assert_echo("\n".join(map(json.dumps, events)), self.message, True)

    def test_polling_fallback_and_nonzero_exit(self):
        output = "\n".join(map(json.dumps, self.stream()))
        with self.assertRaisesRegex(ValueError, "polling"):
            check_response(subprocess.CompletedProcess([], 0, output, "falling back to polling"), self.message, True)
        with self.assertRaisesRegex(ValueError, "exited"):
            check_response(subprocess.CompletedProcess([], 1, output, ""), self.message, True)

    def test_empty_or_invalid_output(self):
        for output in ("", "not JSON", "[]", json.dumps(self.task()) + "garbage"):
            with self.subTest(output=output), self.assertRaises(ValueError):
                assert_echo(output, self.message)


if __name__ == "__main__":
    unittest.main()
