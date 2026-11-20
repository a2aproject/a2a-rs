# Copyright AGNTCY Contributors (https://github.com/agntcy)
# Copyright A2A Contributors (https://github.com/a2aproject)
# SPDX-License-Identifier: Apache-2.0
"""Check the real Go CLI -> Rust plugin -> gateway -> Go echo-agent path.

The caller starts the gateway and echo agent and sets A2A_SLIMRPC_PLUGIN_CONFIG.
Only the Python standard library is required. Raw output is kept for CI diagnosis.
"""

import argparse
import json
from pathlib import Path
import subprocess
import uuid


COMPLETED = {"completed", "TASK_STATE_COMPLETED"}


def assert_echo(output, message, streaming=False):
    """Reject request-history echoes, incomplete tasks and truncated streams."""
    decoder = json.JSONDecoder()
    events = []
    remaining = output.strip()
    while remaining:
        event, end = decoder.raw_decode(remaining)
        if not isinstance(event, dict):
            raise ValueError("Expected a JSON response object")
        events.append(event)
        remaining = remaining[end:].strip()
    if not events:
        raise ValueError("Empty CLI response")

    artifacts = []
    for event in events:
        task = event.get("task", event)
        artifacts.extend(task.get("artifacts", []))
        update = event.get("artifactUpdate", event)
        if "artifact" in update:
            artifacts.append(update["artifact"])
    if not any(part.get("text") == message
               for artifact in artifacts for part in artifact.get("parts", [])):
        raise ValueError("Expected response artifact text missing")

    terminal = events[-1].get("statusUpdate", events[-1]) if streaming else events[-1].get("task", events[-1])
    if terminal.get("status", {}).get("state") not in COMPLETED:
        raise ValueError("Final completed task status missing")
    if streaming and terminal.get("final", True) is not True:
        raise ValueError("Streaming response is not final")


def check_response(result, message, streaming=False):
    if result.returncode:
        raise ValueError(f"CLI exited with {result.returncode}")
    if streaming and "falling back to polling" in result.stderr.lower():
        raise ValueError("CLI fell back to polling instead of streaming")
    assert_echo(result.stdout, message, streaming)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    for streaming in (False, True):
        mode = "streaming" if streaming else "unary"
        message = f"slimrpc-interop-{mode}-{uuid.uuid4()}"
        command = ["a2a", "send", "--verbose", "--output", "jsonl" if streaming else "json",
                   "--timeout", "20s", "--transport", "slimrpc",
                   "--endpoint", "slim://agntcy/demo/echo_agent", message]
        if streaming:
            command.append("--stream")
        # An outer timeout also catches a plugin or CLI that fails to terminate.
        with (args.output_dir / f"{mode}.stdout").open("w+") as stdout, \
                (args.output_dir / f"{mode}.stderr").open("w+") as stderr:
            subprocess_result = subprocess.run(command, stdout=stdout, stderr=stderr, timeout=35)
            stdout.seek(0)
            stderr.seek(0)
            result = subprocess.CompletedProcess(command, subprocess_result.returncode, stdout.read(), stderr.read())
        check_response(result, message, streaming)
        print(f"PASS: {mode} echoed the response artifact and completed", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
