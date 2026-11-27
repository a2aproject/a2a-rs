#!/usr/bin/env bash
# Copyright AGNTCY Contributors (https://github.com/agntcy)
# Copyright A2A Contributors (https://github.com/a2aproject)
# SPDX-License-Identifier: Apache-2.0
#
# Live round trip: Go a2a CLI -> this plugin -> SLIM gateway -> Go echo agent.
#
# Starts the gateway container and the echo agent, bounds their startup, then
# runs the assertions in slimrpc_interop.py. Sends are never retried. CI runs
# this script; it also runs locally from any directory.
#
# Needs on PATH: docker, python3, a2a, a2a-transport-slimrpc and
# slimrpc-echo-agent (the echo agent built from agntcy/slim-a2a-go).
#
# Environment:
#   SLIM_GATEWAY_IMAGE         gateway image, required (CI pins it by digest)
#   A2A_SLIMRPC_PLUGIN_CONFIG  plugin config, defaults to the checked-in example
#   INTEROP_LOG_DIR            output directory, defaults to ./interop-logs
#   STARTUP_TIMEOUT            seconds allowed for each component, default 60

set -euo pipefail

: "${SLIM_GATEWAY_IMAGE:?set SLIM_GATEWAY_IMAGE to the SLIM gateway image}"

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
log_dir=${INTEROP_LOG_DIR:-interop-logs}
startup_timeout=${STARTUP_TIMEOUT:-60}
port=46357
gateway="a2a-slimrpc-interop-$$"
export A2A_SLIMRPC_PLUGIN_CONFIG="${A2A_SLIMRPC_PLUGIN_CONFIG:-$root/a2acli-transport-slimrpc/examples/plugin.yaml}"

for tool in docker python3 a2a a2a-transport-slimrpc slimrpc-echo-agent; do
  if ! command -v "$tool" >/dev/null; then
    echo "error: $tool is not on PATH" >&2
    exit 1
  fi
done

mkdir -p "$log_dir"
agent_pid=""
cleanup() {
  if [ -n "$agent_pid" ]; then
    kill "$agent_pid" 2>/dev/null || true
    wait "$agent_pid" 2>/dev/null || true
  fi
  docker logs "$gateway" > "$log_dir/gateway.log" 2>&1 || true
  docker rm -f "$gateway" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run -d --name "$gateway" \
  -p "127.0.0.1:$port:$port" \
  -v "$root/a2acli-transport-slimrpc/examples/slim-gateway.yaml:/config.yaml:ro" \
  "$SLIM_GATEWAY_IMAGE" /slim --config /config.yaml >/dev/null

if ! timeout "$startup_timeout" bash -c \
    "until : >/dev/tcp/127.0.0.1/$port; do sleep 1; done" 2>/dev/null; then
  echo "error: SLIM gateway did not accept connections on port $port within ${startup_timeout}s" >&2
  exit 1
fi

# The host port can accept TCP before SLIM accepts gRPC, so restart the agent
# until it reports ready. Each attempt keeps its own log; only the current one
# is inspected.
agent_log=""
ready=false
for attempt in $(seq 1 "$startup_timeout"); do
  if [ -z "$agent_pid" ] || ! kill -0 "$agent_pid" 2>/dev/null; then
    if [ -n "$agent_pid" ]; then
      wait "$agent_pid" 2>/dev/null || true
    fi
    agent_log="$log_dir/agent-attempt-$attempt.log"
    echo "Starting echo agent (readiness attempt $attempt)" > "$agent_log"
    GOLANG_PROTOBUF_REGISTRATION_CONFLICT=warn \
      slimrpc-echo-agent --a2a-version v1 --slim-endpoint "http://127.0.0.1:$port" \
      >> "$agent_log" 2>&1 &
    agent_pid=$!
  fi
  if grep -q 'echo agent ready' "$agent_log"; then
    ready=true
    break
  fi
  sleep 1
done
if [ "$ready" != true ] || ! kill -0 "$agent_pid" 2>/dev/null; then
  echo "error: echo agent was not ready within ${startup_timeout}s; see $log_dir/agent-attempt-*.log" >&2
  exit 1
fi

python3 "$root/scripts/slimrpc_interop.py" --output-dir "$log_dir"
