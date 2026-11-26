# a2a-collab

Transport-agnostic relay logic for the [A2A collaborative task extension](https://github.com/a2aproject/experimental-cpb-slimrpc/blob/main/spec/v1/a2a-collaborative-task.md).

## Overview

Provides two functions used by relay implementations (CLI, servers, transports):

- **`translate_peer_response`** — translates a peer agent's `StreamResponse` into a
  `SendMessageRequest` per spec §5.1, with session ID rewriting and shared-task attribution.
- **`require_collaborative_extensions`** — pre-flight check that an `AgentCard` declares
  both required extension URIs before starting a collaborative session.

## Extension URIs

The required URIs are defined in the `a2a` crate:

- `EXTENSION_URI_SHARED_TASK` — `https://a2a-protocol.org/extensions/shared-task/v1`
- `EXTENSION_URI_COLLABORATIVE_TASK` — `https://a2a-protocol.org/extensions/collaborative-task/v1`

## v1.0 limitations

- `StreamResponse::ArtifactUpdate` is translated to a `Part::data` envelope
  (`application/vnd.a2a.task-artifact-update+json`) because `SendMessageRequest.artifact_updates`
  is not part of the A2A v1.0 wire format. This will be updated in v1.1.
- `TaskMessageUpdateEvent` is not yet part of the spec and is deferred to v1.1.
