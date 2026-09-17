# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.2](https://github.com/a2aproject/a2a-rs/compare/a2a-lf-v0.3.1...a2a-lf-v0.3.2) - 2026-09-17

### Added

- *(fuzz)* protojson round trip and Agent Card fuzzing (#238 Phase 2) ([#262](https://github.com/a2aproject/a2a-rs/pull/262))

### Other

- add the A2A Contributors copyright line to every header ([#260](https://github.com/a2aproject/a2a-rs/pull/260))

## [0.3.1](https://github.com/a2aproject/a2a-rs/compare/a2a-lf-v0.3.0...a2a-lf-v0.3.1) - 2026-09-14

### Added

- *(a2a-server)* serve the extended Agent Card, with its own error code ([#212](https://github.com/a2aproject/a2a-rs/pull/212))

### Added

- `A2AError::extended_card_not_configured()`

### Fixed

- map `EXTENDED_CARD_NOT_CONFIGURED` and `EXTENSION_SUPPORT_REQUIRED` to
  HTTP 400; both previously fell through to 500, contradicting the
  FAILED_PRECONDITION gRPC status a2a-server already assigned them

## [0.3.0](https://github.com/a2aproject/a2a-rs/compare/a2a-lf-v0.2.4...a2a-lf-v0.3.0) - 2026-05-11

### Fixed

- use TaskPushNotificationConfig v1.0.0 ([#66](https://github.com/a2aproject/a2a-rs/pull/66))
- align error code mappings with upstream spec fix (A2A#1627) ([#58](https://github.com/a2aproject/a2a-rs/pull/58))

## [0.2.4](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-v0.2.3...agntcy-a2a-v0.2.4) - 2026-04-05

### Fixed

- normalize gRPC agent card URLs

## [0.2.3](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-v0.2.2...agntcy-a2a-v0.2.3) - 2026-04-04

### Added

- add SLIMRPC transport binding

### Fixed

- drop dotted jsonrpc aliases
- align jsonrpc and agent-card interop

## [0.2.2](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-v0.2.1...agntcy-a2a-v0.2.2) - 2026-04-03

### Other

- add crate rustdoc for agntcy-a2a

## [0.2.1](https://github.com/a2aproject/a2a-rs/compare/agntcy-a2a-v0.2.0...agntcy-a2a-v0.2.1) - 2026-04-03

### Other

- add crate readmes for crates.io

## [0.2.0](https://github.com/a2aproject/a2a-rs/compare/a2a-v0.1.0...a2a-v0.2.0) - 2026-04-03

### Added

- initialize a2a-rs Rust SDK
