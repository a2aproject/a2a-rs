# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0](https://github.com/a2aproject/a2a-rs/compare/a2a-server-lf-v0.4.4...a2a-server-lf-v0.5.0) - 2026-09-17

### Added

- *(fuzz)* Phase 3 validation targets + weekly run ([#238](https://github.com/a2aproject/a2a-rs/pull/238)) ([#264](https://github.com/a2aproject/a2a-rs/pull/264))

### Fixed

- *(a2a-server)* [**breaking**] enforce the spec's taskId and contextId rules ([#249](https://github.com/a2aproject/a2a-rs/pull/249))
- *(a2a-server)* [**breaking**] require TaskStore::begin_cancel instead of defaulting it ([#252](https://github.com/a2aproject/a2a-rs/pull/252))
- *(server)* protect terminal task states and make cancel transition atomic ([#126](https://github.com/a2aproject/a2a-rs/pull/126))

### Other

- add the A2A Contributors copyright line to every header ([#260](https://github.com/a2aproject/a2a-rs/pull/260))

## [0.4.4](https://github.com/a2aproject/a2a-rs/compare/a2a-server-lf-v0.4.3...a2a-server-lf-v0.4.4) - 2026-09-14

### Added

- *(a2a-server)* serve the extended Agent Card, with its own error code ([#212](https://github.com/a2aproject/a2a-rs/pull/212))

### Fixed

- *(a2a-server)* bound list responses for every store, not just the in-tree ones ([#226](https://github.com/a2aproject/a2a-rs/pull/226))
- *(a2a-server)* close the trailing-dot bypass in the push URL blocklist ([#225](https://github.com/a2aproject/a2a-rs/pull/225))
- *(server)* harden push notifications (URL validation, CRLF credentials, per-task config cap) ([#124](https://github.com/a2aproject/a2a-rs/pull/124))
- *(server)* validate list task inputs (status, pageSize, pageToken) ([#122](https://github.com/a2aproject/a2a-rs/pull/122))
- *(a2a-server)* enforce the capabilities the Agent Card declares ([#205](https://github.com/a2aproject/a2a-rs/pull/205))
- *(a2a-server)* SSE event ids, keep-alive, and a request body limit ([#202](https://github.com/a2aproject/a2a-rs/pull/202))
- *(a2a-pb)* ignore unknown fields in ProtoJSON decode (spec §5.7) ([#81](https://github.com/a2aproject/a2a-rs/pull/81))

### Other

- *(a2a-server)* pin that artifacts do not aggregate across tasks in one context ([#211](https://github.com/a2aproject/a2a-rs/pull/211))
- add authorize callback and align historyLength ([#196](https://github.com/a2aproject/a2a-rs/pull/196))

### Added

- `with_extended_agent_card` / `with_extended_agent_card_resolver` on
  `DefaultRequestHandler`, so a server can actually serve an extended Agent
  Card; configuring one also declares the capability

### Fixed

- report `EXTENDED_CARD_NOT_CONFIGURED` (-32007) when no extended card is
  configured, instead of `UNSUPPORTED_OPERATION` (-32004)

### Fixed

- apply `historyLength` on `get_task` and clamp `<= 0` on both get and list
- treat `cancel_task` on an already-`CANCELED` task as idempotent; other terminal states stay `TASK_NOT_CANCELABLE`
- assign a monotonic `id` to every SSE event, so a client can detect a gap
  and has a value for `Last-Event-ID`
- keep SSE responses alive with a 15s comment frame, so idle subscriptions
  are not dropped by intermediaries
- bound request bodies on both bindings at 10 MB, rejecting oversized ones
  with `413` instead of reading them into memory
- enforce the `AgentCapabilities` a server declares: `streaming: false` and
  `push_notifications: false` now refuse the corresponding operations instead
  of being stored and ignored. An undeclared (`None`) capability stays
  permissive, and supplying a push store no longer overrides an explicit
  `false`

### Added

- `RequestAuthorizer` callback on `DefaultRequestHandler` (`with_authorizer`)

## [0.4.3](https://github.com/a2aproject/a2a-rs/compare/a2a-server-lf-v0.4.2...a2a-server-lf-v0.4.3) - 2026-08-27

### Fixed

- *(server)* avoid buffering without subscribers ([#147](https://github.com/a2aproject/a2a-rs/pull/147))

## [0.4.2](https://github.com/a2aproject/a2a-rs/compare/a2a-server-lf-v0.4.1...a2a-server-lf-v0.4.2) - 2026-08-26

### Other

- clamp the page token offset when listing tasks ([#140](https://github.com/a2aproject/a2a-rs/pull/140))

## [0.4.1](https://github.com/a2aproject/a2a-rs/compare/a2a-server-lf-v0.4.0...a2a-server-lf-v0.4.1) - 2026-07-16

### Other

- updated the following local packages: a2a-pb

## [0.4.0](https://github.com/a2aproject/a2a-rs/compare/a2a-server-lf-v0.3.1...a2a-server-lf-v0.4.0) - 2026-05-27

### Fixed

- Upgrade to reqwest 0.13 and refactor TLS feature flags ([#78](https://github.com/a2aproject/a2a-rs/pull/78))
- a2a-client-lf fails to compile with --no-default-features ([#75](https://github.com/a2aproject/a2a-rs/pull/75))

## [0.3.1](https://github.com/a2aproject/a2a-rs/compare/a2a-server-lf-v0.3.0...a2a-server-lf-v0.3.1) - 2026-05-22

### Fixed

- *(a2a-server)* propagate HTTP headers into ServiceParams ([#70](https://github.com/a2aproject/a2a-rs/pull/70))

## [0.3.0](https://github.com/a2aproject/a2a-rs/compare/a2a-server-lf-v0.2.7...a2a-server-lf-v0.3.0) - 2026-05-11

### Fixed

- use TaskPushNotificationConfig v1.0.0 ([#66](https://github.com/a2aproject/a2a-rs/pull/66))
- align error code mappings with upstream spec fix (A2A#1627) ([#58](https://github.com/a2aproject/a2a-rs/pull/58))

## [0.2.7](https://github.com/a2aproject/a2a-rs/compare/a2a-server-lf-v0.2.6...a2a-server-lf-v0.2.7) - 2026-04-30

### Added

- built-in TLS (rustls) support for transport factories ([#56](https://github.com/a2aproject/a2a-rs/pull/56))
- use rustls everywhere and expose TLS backend selection via feature flags ([#47](https://github.com/a2aproject/a2a-rs/pull/47))

## [0.2.6](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-server-v0.2.5...agntcy-a2a-server-v0.2.6) - 2026-04-06

### Other

- apply rustfmt to interop changes ([#33](https://github.com/a2aproject/a2a-rs/pull/33))

## [0.2.5](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-server-v0.2.4...agntcy-a2a-server-v0.2.5) - 2026-04-06

### Fixed

- *(interop)* align push-config JSON bindings with a2a-go ([#31](https://github.com/a2aproject/a2a-rs/pull/31))

## [0.2.4](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-server-v0.2.3...agntcy-a2a-server-v0.2.4) - 2026-04-06

### Added

- *(a2a-server)* support resumable subscriptions and push delivery

## [0.2.3](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-server-v0.2.2...agntcy-a2a-server-v0.2.3) - 2026-04-06

### Added

- *(server)* support push configs and gRPC list interop

## [0.2.2](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-server-v0.2.1...agntcy-a2a-server-v0.2.2) - 2026-04-05

### Other

- updated the following local packages: agntcy-a2a, agntcy-a2a-pb

## [0.2.1](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-server-v0.2.0...agntcy-a2a-server-v0.2.1) - 2026-04-05

### Added

- use ProtoJSON for JSON wire payloads

### Other

- format ProtoJSON transport changes

## [0.2.0](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-server-v0.1.3...agntcy-a2a-server-v0.2.0) - 2026-04-04

### Fixed

- align HTTP+JSON REST interop

## [0.1.3](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-server-v0.1.2...agntcy-a2a-server-v0.1.3) - 2026-04-04

### Fixed

- drop dotted jsonrpc aliases
- align jsonrpc and agent-card interop

## [0.1.2](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-server-v0.1.1...agntcy-a2a-server-v0.1.2) - 2026-04-03

### Other

- updated the following local packages: agntcy-a2a

## [0.1.1](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-server-v0.1.0...agntcy-a2a-server-v0.1.1) - 2026-04-03

### Other

- add crate readmes for crates.io

## [0.1.0](https://github.com/a2aproject/a2a-rs/releases/tag/a2a-server-v0.1.0) - 2026-04-03

### Added

- initialize a2a-rs Rust SDK
