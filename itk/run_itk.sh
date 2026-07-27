#!/bin/bash
# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0
set -ex

# Always run from the directory containing this script so relative paths work
# regardless of where the caller invokes it from.
cd "$(dirname "${BASH_SOURCE[0]}")"

export ITK_LOG_LEVEL="${ITK_LOG_LEVEL:-INFO}"
RESULT=1

cleanup() {
  set +x
  echo "Cleaning up artifacts..."
  docker stop itk-service > /dev/null 2>&1 || true
  docker rm itk-service > /dev/null 2>&1 || true
  # Preserve the image so layer caching speeds up subsequent local runs.
  # Set ITK_REMOVE_IMAGE=1 to force removal (e.g. in CI cleanup jobs).
  if [ "${ITK_REMOVE_IMAGE:-0}" = "1" ]; then
    docker rmi itk_service > /dev/null 2>&1 || true
  fi
  rm -rf a2a-itk > /dev/null 2>&1 || true
  echo "Done. Final exit code: $RESULT"
}
trap cleanup EXIT

: "${A2A_ITK_REVISION:?A2A_ITK_REVISION environment variable must be set}"

if [ ! -d "a2a-itk" ]; then
  git clone https://github.com/a2aproject/a2a-itk.git a2a-itk
fi
cd a2a-itk
git fetch origin
git checkout "$A2A_ITK_REVISION"
if git symbolic-ref -q HEAD > /dev/null; then
  git pull origin "$A2A_ITK_REVISION"
fi
cd ..

# Build the ITK service Docker image (polyglot: includes Rust 1.85, Go, Python, Node, etc.)
docker build -t itk_service a2a-itk

A2A_RS_ROOT=$(cd .. && pwd)
ITK_DIR=$(pwd)

docker rm -f itk-service || true

DOCKER_MOUNT_LOGS=""
if [ "${ITK_LOG_LEVEL^^}" = "DEBUG" ]; then
  mkdir -p "$ITK_DIR/logs"
  DOCKER_MOUNT_LOGS="-v $ITK_DIR/logs:/app/logs"
fi

docker run -d --name itk-service \
  -v "$A2A_RS_ROOT:/app/agents/repo" \
  -v "$ITK_DIR:/app/agents/repo/itk" \
  $DOCKER_MOUNT_LOGS \
  -e ITK_LOG_LEVEL="$ITK_LOG_LEVEL" \
  -p 8000:8000 \
  itk_service

docker exec -u root itk-service git config --system --add safe.directory /app/agents/repo
docker exec -u root itk-service git config --system --add safe.directory /app/agents/repo/itk
docker exec -u root itk-service git config --system core.multiPackIndex false

MAX_RETRIES=30
echo "Waiting for ITK service to start on 127.0.0.1:8000..."
set +e
for i in $(seq 1 $MAX_RETRIES); do
  if curl -s http://127.0.0.1:8000/ > /dev/null; then
    echo "Service is up!"
    break
  fi
  echo "Still waiting... ($i/$MAX_RETRIES)"
  sleep 2
done

if ! curl -s http://127.0.0.1:8000/ > /dev/null; then
  echo "Error: ITK service failed to start on port 8000"
  docker logs itk-service
  exit 1
fi

SCENARIO_FILE="scenarios.json"
if [ "${ITK_NIGHTLY_RUN^^}" = "TRUE" ]; then
  SCENARIO_FILE="scenarios_full.json"
fi

echo "ITK Service is up! Sending compatibility test request using $SCENARIO_FILE..."
RESPONSE=$(curl -s -X POST http://127.0.0.1:8000/run \
  -H "Content-Type: application/json" \
  -d "@$SCENARIO_FILE")

# Validate the ITK response shape BEFORE branching into nightly/CI post-processing.
# Guards against both branches silently accepting a malformed or error response:
# - CI mode: prevents an unhandled AttributeError inside the summariser.
# - Nightly mode: prevents the metrics processor from producing an empty/invalid
#   history entry that would then be uploaded to the release asset.
echo "$RESPONSE" | python3 -c "
import sys, json
raw = sys.stdin.read()
try:
    data = json.loads(raw)
except json.JSONDecodeError as e:
    print(f'ERROR: could not parse ITK response JSON: {e}', file=sys.stderr)
    print(f'Raw response: {raw}', file=sys.stderr)
    sys.exit(1)
if not isinstance(data, dict):
    print(f'ERROR: ITK response is not a JSON object (got {type(data).__name__})', file=sys.stderr)
    sys.exit(1)
if 'detail' in data:
    print(f'ERROR: ITK service returned an error: {data[\"detail\"]}', file=sys.stderr)
    sys.exit(1)
if 'results' not in data:
    print(f'ERROR: ITK response missing \"results\" field. Keys: {list(data.keys())}', file=sys.stderr)
    sys.exit(1)
if not isinstance(data['results'], dict):
    print(f'ERROR: ITK response \"results\" field is not an object (got {type(data[\"results\"]).__name__})', file=sys.stderr)
    sys.exit(1)
"
RESULT=$?
if [ $RESULT -ne 0 ]; then
  echo "ITK response failed validation. Container logs:"
  docker logs itk-service
  exit $RESULT
fi

if [ "${ITK_NIGHTLY_RUN^^}" = "TRUE" ]; then
  echo "Nightly run detected. Saving raw results and running process_results.py..."
  echo "$RESPONSE" > raw_results.json
  python3 a2a-itk/scripts/process_results.py \
    --history_output_file itk_rust.json \
    --history_url https://github.com/a2aproject/a2a-rs/releases/download/nightly-metrics/itk_rust.json
  RESULT=$?
else
  echo "--------------------------------------------------------"
  echo "ITK TEST RESULTS:"
  echo "--------------------------------------------------------"
  echo "$RESPONSE" | python3 -c "
import sys, json
data = json.loads(sys.stdin.read())
all_passed = data.get('all_passed', False)
for name, value in data['results'].items():
    if isinstance(value, dict):
        passed = bool(value.get('passed', False))
    elif isinstance(value, bool):
        passed = value
    else:
        passed = False
    print(f'{name}: {\"PASSED\" if passed else \"FAILED\"}')
print('--------------------------------------------------------')
print(f'OVERALL STATUS: {\"PASSED\" if all_passed else \"FAILED\"}')
if not all_passed:
    sys.exit(1)
"
  RESULT=$?
fi
set -e

if [ $RESULT -ne 0 ]; then
  echo "Tests failed. Container logs:"
  docker logs itk-service
fi
echo "--------------------------------------------------------"

exit $RESULT
