# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-05-06

### Added

- Add WebSocket custom protocol binding for A2A v1, including:
  - `WebSocketTransport` and `WebSocketTransportFactory` implementing the
    `a2a_client::transport::Transport` and `TransportFactory` traits.
  - `websocket_router` adapting an `a2a_server::RequestHandler` to an
    `axum::Router` that serves the binding over a persistent WebSocket
    connection with full bidirectional streaming and multiplexing.
  - JSON envelope types (`WsRequestEnvelope`, `WsResponseEnvelope`,
    `WsErrorObject`) implementing the wire format defined in the
    A2A WebSocket binding specification.
  - Mapping helpers between `A2AError` and the canonical WebSocket
    error type strings, plus close-code selection for fatal failures.
  - Service parameter handling that combines connection-scoped headers
    with per-request `serviceParams` (per-request takes precedence).
- Re-export `TRANSPORT_PROTOCOL_WEBSOCKET` from the `a2a` core crate so
  application code can register the factory using the shared constant.
