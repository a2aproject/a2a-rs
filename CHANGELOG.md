# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This is the workspace changelog. Versions are `a2a-cli`'s, which sits at the top
of the dependency graph, and the entries include changes from every crate. Each
library also keeps its own: [a2a](a2a/CHANGELOG.md),
[a2a-client](a2a-client/CHANGELOG.md), [a2a-server](a2a-server/CHANGELOG.md),
[a2a-pb](a2a-pb/CHANGELOG.md), [a2a-grpc](a2a-grpc/CHANGELOG.md),
[a2a-slimrpc](a2a-slimrpc/CHANGELOG.md).

## [Unreleased]

## [0.2.1](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.2.0...a2a-cli-v0.2.1) - 2026-09-15

### Added

- *(fuzz)* fuzz the SSE framing path ([#239](https://github.com/a2aproject/a2a-rs/pull/239))

### Other

- generate the workspace CHANGELOG at the repo root ([#243](https://github.com/a2aproject/a2a-rs/pull/243))

## [0.2.0](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.11...a2a-cli-v0.2.0) - 2026-09-14

### Added

- *(release)* sign the published CLI archives ([#216](https://github.com/a2aproject/a2a-rs/pull/216))
- *(a2acli)* report a malformed invocation as an error envelope ([#194](https://github.com/a2aproject/a2a-rs/pull/194))
- *(a2acli)* name a non-success or paused task outcome on stderr ([#192](https://github.com/a2aproject/a2a-rs/pull/192))
- *(a2acli)* --a2a-version flag and 1.x-bounded version negotiation ([#191](https://github.com/a2aproject/a2a-rs/pull/191))
- *(a2acli)* Agent Card reference resolution and direct --endpoint ([#190](https://github.com/a2aproject/a2a-rs/pull/190))
- *(a2acli)* overridable defaults, config precedence, and config show ([#177](https://github.com/a2aproject/a2a-rs/pull/177))
- *(a2acli)* auth, transport selection, and version-negotiation flags ([#176](https://github.com/a2aproject/a2a-rs/pull/176))
- *(a2acli)* text output mode, error envelope, and exit-code contract ([#173](https://github.com/a2aproject/a2a-rs/pull/173))
- *(a2acli)* blocking-by-default send, task polling, and message parts ([#172](https://github.com/a2aproject/a2a-rs/pull/172))
- *(a2acli)* [**breaking**] align command surface with the a2a-cli taxonomy ([#171](https://github.com/a2aproject/a2a-rs/pull/171))

### Fixed

- *(release)* use the cosign v3 bundle format when signing archives ([#230](https://github.com/a2aproject/a2a-rs/pull/230))
- *(a2a-client)* echo the selected interface's tenant on every request ([#200](https://github.com/a2aproject/a2a-rs/pull/200))

### Other

- *(a2acli)* verify stateless interaction-id handling (INTERACT_001-005) ([#174](https://github.com/a2aproject/a2a-rs/pull/174))

## [0.1.11](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.10...a2a-cli-v0.1.11) - 2026-08-27

### Other

- updated the following local packages: a2a-client-lf, a2a-server-lf

## [0.1.10](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.9...a2a-cli-v0.1.10) - 2026-08-26

### Other

- updated the following local packages: a2a-server-lf

## [0.1.9](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.8...a2a-cli-v0.1.9) - 2026-08-25

### Other

- update Cargo.lock dependencies

## [0.1.8](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.7...a2a-cli-v0.1.8) - 2026-08-05

### Other

- update Cargo.lock dependencies

## [0.1.7](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.6...a2a-cli-v0.1.7) - 2026-07-16

### Other

- update Cargo.lock dependencies

## [0.1.6](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.5...a2a-cli-v0.1.6) - 2026-06-25

### Other

- *(a2acli)* bump version to 0.1.6 ([#89](https://github.com/a2aproject/a2a-rs/pull/89))

## [0.1.5](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.4...a2a-cli-v0.1.5) - 2026-05-27

### Fixed

- Upgrade to reqwest 0.13 and refactor TLS feature flags ([#78](https://github.com/a2aproject/a2a-rs/pull/78))

## [0.1.4](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.3...a2a-cli-v0.1.4) - 2026-05-22

### Other

- updated the following local packages: a2a-client-lf, a2a-server-lf

## [0.1.3](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.2...a2a-cli-v0.1.3) - 2026-05-11

### Fixed

- use TaskPushNotificationConfig v1.0.0 ([#66](https://github.com/a2aproject/a2a-rs/pull/66))

## [0.1.2](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.1...a2a-cli-v0.1.2) - 2026-04-30

### Other

- updated the following local packages: a2a-client-lf

## [0.1.1](https://github.com/a2aproject/a2a-rs/compare/a2a-cli-v0.1.0...a2a-cli-v0.1.1) - 2026-04-30

### Added

- built-in TLS (rustls) support for transport factories ([#56](https://github.com/a2aproject/a2a-rs/pull/56))
- use rustls everywhere and expose TLS backend selection via feature flags ([#47](https://github.com/a2aproject/a2a-rs/pull/47))

## [0.1.0](https://github.com/a2aproject/a2a-rs/releases/tag/a2a-cli-v0.1.0) - 2026-04-14

### Added

- migrate A2A crates to a2a-lf namespace ([#41](https://github.com/a2aproject/a2a-rs/pull/41))
- *(a2a-client)* make A2AClient generic over Transport to enable zero-cost static dispatch ([#43](https://github.com/a2aproject/a2a-rs/pull/43))
- add standalone a2acli CLI ([#38](https://github.com/a2aproject/a2a-rs/pull/38))

### Added

- add standalone `a2acli` binary crate
- add task push notification config CRUD commands
- make the package installable as `a2a-cli`