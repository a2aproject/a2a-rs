#!/bin/bash
# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0
#
# ITK harness for a2a-rs — a thin shim over a2a-itk's shared driver.
#
# Everything that used to live here (clone, image build, container start,
# readiness poll, response validation, POST /run, result reporting, nightly
# metrics) is now in a2a-itk/scripts/run_itk_shared.sh, which all five SDK
# repos share. The response validation and the keep-the-image default that
# this repo pioneered are part of that shared driver now, so every SDK gets
# them.
#
# Scenarios come from the shared role-based set in a2a-itk rather than a
# scenarios.json in this repo — see a2a-itk/scenarios/traversal/.
set -e
cd "$(dirname "${BASH_SOURCE[0]}")"

ITK_SDK_NAME=rust
# The repo doesn't follow the a2a-<sdk> pattern the default assumes.
ITK_SDK_REPO=a2a-rs
ITK_SCENARIO_SET=shared

# No codegen step: build.rs generates from instruction.proto in the a2a-itk
# checkout directly, so there is nothing to copy.
ITK_COPY_PROTO=0

# --- bootstrap -------------------------------------------------------------
# The shared driver lives in a2a-itk, so the checkout has to exist before it
# can be sourced. CI has already placed it here via actions/checkout; locally
# we clone it from a2aproject/a2a-itk.
: "${A2A_ITK_REVISION:?A2A_ITK_REVISION environment variable must be set}"
if [ ! -d a2a-itk ]; then
  git clone https://github.com/a2aproject/a2a-itk.git a2a-itk
  git -C a2a-itk checkout "$A2A_ITK_REVISION"
fi

source a2a-itk/scripts/run_itk_shared.sh
