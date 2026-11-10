# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Aligned with the current binding specification (breaking).**
  - The transport identifier is now
    `https://a2a-protocol.org/bindings/experimental-websocket/v1` and is
    defined by this crate as `TRANSPORT_PROTOCOL_WEBSOCKET`; the core crate no
    longer carries a `WEBSOCKET` constant.
  - Control messages use the reserved `ws/` method prefix. `ws/StreamEnd`,
    `ws/CancelStream`, and `ws/ReauthenticationRequired` are JSON-RPC
    notifications carrying their data in `params`; the `streamEnd`,
    `cancelStream`, `control`, `reason`, and `retryAfterMs` envelope members
    are removed.
  - `Authenticate` is now `ws/Authenticate`, and its params are a
    `credentials` map keyed by Agent Card `securitySchemes` names
    (`AuthenticateParams::single`). The result carries an optional
    `expiresAt`. A refresh must keep the same principal (`WS_PERMISSION_DENIED`
    otherwise), an unknown scheme is `-32602`, three failed attempts close
    with `1008`, and a server without in-band refresh answers `-32601`.
    `WsAuthenticator::refresh` takes the credentials map, and
    `accepted_schemes` feeds the `schemes` error metadata.
  - `AuthStatus::ReauthRequired` and `WsAuthenticator::reauth_grace` are
    replaced by `AuthStatus::ExpiresAt { expires_at }`. The server sends
    `ws/ReauthenticationRequired` once and closes with `4001` at `expiresAt`
    unless the credentials are refreshed. On expiry, the current request and
    every active stream fail with `WS_CREDENTIALS_EXPIRED` before the close.
  - Binding-specific errors use `-32000` with an `ErrorInfo` reason from
    `binding_errors` (`WS_UNAUTHENTICATED`, `WS_CREDENTIALS_EXPIRED`,
    `WS_PERMISSION_DENIED`, `WS_RATE_LIMITED`, `WS_TOO_MANY_IN_FLIGHT`);
    `binding_error` and `binding_error_reason` build and read them.
  - `A2A-Version` is connection-scoped. The server reads it from the handshake
    headers or `A2A-` query parameters, rejects an absent or unsupported
    version with `400`, and answers a request naming a different version with
    `-32009`. The client sends it (`ConnectOptions::with_protocol_version`,
    defaulting to the interface's `protocolVersion` when built by the factory).
  - Invalid JSON is answered with `-32700` and the connection stays open;
    repeated malformed messages close with `4000`. Invalid UTF-8 closes with
    `1007`. Notifications are never answered, and unknown or malformed ones
    are ignored.
  - `serviceParams` keys that differ only in case are rejected with `-32600`.

### Added

- **Keep-alive pings and idle timeouts (spec Section 2.2), previously absent.**
  `WebSocketConfig::liveness` takes a `Liveness { ping_interval, pong_timeout,
  idle_timeout }`. A monitor task pings on the interval, closes with `1001`
  (Going Away) when the pong does not arrive in time, and closes with `1001`
  when the connection has carried no application-level message for
  `idle_timeout`. Outbound frames count as activity as well as inbound ones, so
  a long-running stream that pushes events while the client stays quiet is
  treated as busy rather than idle. On by default with the intervals Section 2.2
  recommends (30s / 10s / 5min); `LivenessPolicy::Disabled` opts out, which
  suits deployments where a proxy already does this.
- **Connection and stream limits (spec Section 8.6), previously absent.**
  `WebSocketConfig::connection_limits` caps concurrent connections per
  authenticated identity and concurrent streams per connection. An identity at
  its connection limit is refused during the upgrade with `503`, so it can back
  off rather than be handed a connection that closes immediately; the slot is
  released however the connection ends. A request that would exceed the stream
  cap is answered with a `-32000` error and reason `WS_TOO_MANY_IN_FLIGHT`,
  leaving the connection and its existing streams intact. Stream
  slots are claimed before the handler is invoked, so the cap is decided under a
  single lock rather than after the work has started. On by default with
  deliberately generous values (256 connections per identity, 128 streams per
  connection), since Section 8.6 recommends no figures;
  `ConnectionLimitPolicy::Disabled` opts out. Registering the stream earlier
  also means a `ws/CancelStream` arriving while the handler is still starting up is
  now honoured instead of missed.
- **Reconnection backoff (spec Section 8.5), previously absent.**
  `WebSocketTransport::connect_with_retry` retries a failed connection on an
  exponential backoff with jitter, defaulting to the schedule Section 8.5
  recommends (1s, doubling, capped at 30s). Failures that would recur are not
  retried: a rejected token, an endpoint that is not a WebSocket server, or a
  sub-protocol the server will not speak fail immediately, so a client cannot
  end up hammering an auth server. `429` and `5xx` are retried, which covers the
  `503` this binding's own server returns for a per-identity connection cap.
  Resubscribing to in-progress tasks after reconnecting stays with the
  application, since only it knows which tasks still matter and whether
  duplicate events are safe to apply twice (Section 8.5 steps 2 and 3).
- **Inbound message rate limiting (spec Section 7.4), previously absent.**
  `WebSocketConfig::rate_limit` takes a `RateLimit { max_messages, window }` and
  enforces it with a token bucket in both scopes the spec recommends: once per
  connection, and once across every connection belonging to the same
  authenticated identity. A connection that exceeds its budget receives a
  JSON-RPC error with code `-32000` and reason `WS_RATE_LIMITED` and is
  then closed with `1008` (Policy Violation). The limit is checked before the
  frame is parsed, so the error carries `"id": null`. Identity buckets that have
  fully refilled are pruned, since a refilled bucket is indistinguishable from a
  new one, which bounds memory without affecting behaviour. The limit is on by
  default (`DEFAULT_RATE_LIMIT`, 100 messages/second) because Section 7.4 makes
  it a MUST; `RateLimitPolicy::Disabled` opts out explicitly, which is only
  appropriate behind a trusted layer that enforces its own limits.
- **Client-side handling of `ws/ReauthenticationRequired` (spec Section 9.5),
  which was previously only logged.** `ConnectOptions::reauth` selects a
  `ReauthPolicy`:
  - `on_reauth_required(callback)` reports the notification (with the server's
    `expiresAt`) so the application can reconnect with fresh credentials.
  - `with_credential_provider(provider)` implements the in-band path: the
    transport asks the `CredentialProvider` for fresh credentials and installs
    them with `ws/Authenticate`, keeping the connection open.
  Leaving `reauth` unset now logs a warning naming both options.

### Fixed

- **Server connections leaked their task and socket when the peer closed
  first.** Teardown dropped its own outbound sender but not the one captured by
  the callback that answers pings and echoes closes, so the outbound channel
  never closed, the writer task never saw the end of its stream, and the
  connection task waited on it forever. Nothing surfaced at the time — the close
  had already been written — but the task, the socket, and anything it held
  stayed alive for the life of the process. Found while testing the per-identity
  connection cap, which never freed a slot.
- **A connection could panic during teardown, silently abandoning in-flight
  requests.** The reader watches the writer task so it stops reading once the
  socket is gone, and then awaited the same `JoinHandle` again on the way out.
  Polling a `JoinHandle` after it has completed panics, and because the
  connection runs in a spawned task the panic was swallowed: pending requests
  were never failed and simply hung. Whether it triggered depended on which of
  the reader and writer finished first, so it did not reproduce reliably.
- **Frames could be corrupted when traffic flowed in both directions at once.**
  Each connection drove reads and writes from a single task, with both branches
  of a `tokio::select!` borrowing the same socket, so whenever an outbound frame
  won the race the in-flight `read_frame` future was dropped.
  `fastwebsockets::read_frame` does not tolerate that: it consumes header bytes
  out of a persistent buffer into local variables, so a cancellation partway
  through a frame leaves the parser positioned mid-frame and the next read
  interprets payload bytes as a frame header. The connection then failed with a
  spurious protocol error or a response that would not parse. Anything large
  enough to span more than one read was affected — messages approaching the size
  cap, slow or lossy links, or a peer that writes the header and payload
  separately — which is why it showed up under load rather than in ordinary use.
  Both the server and the client now split the socket, giving the read half and
  the write half a task each, so no read is ever cancelled. The one remaining
  `select!` over a read, in the client, is terminal: it stops reading for good
  rather than resuming against a corrupted parser.

  This also removes the head-of-line blocking that came with the old structure.
  Reads and writes now progress concurrently instead of taking turns, and
  because outbound frames no longer take priority over inbound ones, a
  `ws/CancelStream` sent while the server is busy streaming is read promptly
  instead of waiting for the outbound queue to drain.
- **A request can no longer overwrite the identity its connection authenticated
  with.** Per-request `serviceParams` were merged over the connection-scoped
  ones unconditionally, including keys the authenticator had established. A
  client that authenticated as one principal could therefore send
  `serviceParams: {"x-tenant": "..."}` and have the request handler act on the
  substituted value. Keys published by the authenticator are now authoritative
  for the lifetime of the connection and per-request entries for them are
  ignored; every other key remains client-settable.
- **Inbound `serviceParams` keys are now lowercased**, matching the handshake
  headers, which arrive normalized. Previously `X-Tenant` from an envelope and
  `x-tenant` from a header landed in the map as two unrelated entries — a
  correctness problem in its own right, and a way around the protection above.
- **An in-band credential refresh now updates the authorization the handler
  sees.** `ws/Authenticate` replaced the connection's `AuthContext` but not the
  service parameters derived from it, so a connection that refreshed into a
  different tenant or scope set kept being authorized as before for the rest
  of its life (spec Section 9.3).
- **A fatal error response is now actually delivered before the connection
  closes.** The read loop dropped anything still queued on the outbound channel
  when it exited, so an expired-credentials error (`4001`) and every other
  response sent before a close were silently discarded and the peer saw only
  a dropped socket. Queued frames are now flushed up to and including the
  Close frame.
- **A successful in-band credential refresh now cancels the pending `4001`
  close.** After signalling `ws/ReauthenticationRequired` the server scheduled
  an unconditional close, so a client that refreshed via `ws/Authenticate` was
  disconnected anyway. The scheduled close is now skipped if the credentials
  were refreshed in the meantime.
- **A Close frame's status code now reaches the caller.** In-flight and
  subsequent requests failed with a generic "websocket connection closed"
  regardless of why the server hung up. Closes carrying `4001`, `1007`, `1008`,
  `1009`, `4000`, and `4002` now produce distinct errors, so an application can
  tell reauthentication from a rate limit or an oversize message.
- **An oversize inbound message now closes the connection with code `1009`
  (Message Too Big) as required by spec Section 3.6.** Previously the read
  error was logged and the socket was dropped without a Close frame, leaving
  the peer to infer the reason. Invalid UTF-8 now closes with `1007`, and other
  RFC 6455 framing violations (bad fragmentation, oversize ping, malformed
  close) close with `1002` (Protocol Error) per the close-code table in Section
  2.3. Errors that
  mean the transport is already gone (EOF, I/O failure) still close silently,
  since there is nowhere to write. The mapping lives in
  `errors::close_code_for_read_error`.

### Changed

- `WebSocketConfig` gained `liveness` and `connection_limits`. Code that builds
  the struct with `..Default::default()` is unaffected; code that lists every
  field must add the two new ones. Both default to enforcing their limits, so
  existing servers pick up keep-alive pings, idle timeouts, and the Section 8.6
  caps on upgrade.
- The `fastwebsockets` dependency now enables its `unstable-split` feature, which
  provides the independent read and write halves each connection needs. The
  feature is marked unstable upstream, so the split API surface is confined to
  the two connection loops.
- `WebSocketConfig` gained `max_frame_bytes: Option<usize>` to configure the
  inbound message size limit; `None` keeps the spec's recommended 1 MiB
  (`DEFAULT_MAX_FRAME_BYTES`), which is now re-exported at the crate root.
- `WebSocketConfig::rate_limit` changed type from `Option<RateLimit>` to
  `RateLimitPolicy` (breaking). A default-constructed config is now conformant
  with Section 7.4 instead of silently unlimited, and switching the limit off
  is a deliberate `RateLimitPolicy::Disabled` rather than an omission.
- `AuthContext::with_param` builds the connection-scoped service parameters that
  carry an authenticated identity to the request handler. `AuthContext::user`
  scopes the per-identity rate limit and is not passed to the handler; the docs
  now spell out which field feeds which consumer.
- Added a runnable application-side example (`websocket-server` /
  `websocket-client` in the workspace `examples` crate) covering handshake
  authentication, identity-aware execution, rate limiting, and both client
  reauthentication policies. `a2a-websocket` is now part of the workspace's
  `default-members`, so it is built and tested by a bare `cargo test`.
- **Wire format now conforms to the A2A WebSocket binding specification
  (breaking).** All frames are JSON-RPC 2.0 objects:
  - Every envelope carries `"jsonrpc": "2.0"`; the server rejects other values
    with `InvalidRequestError`.
  - Request `id` may be a string or a number (`JsonRpcId`).
  - Errors use the standard JSON-RPC `error` object with the numeric A2A
    `code` and structured `data` (the previous `{ type, details }` object and
    `WsErrorObject` type are removed).
  - Streaming chunks are delivered in the `result` member (the separate
    `event` member is removed).
  - The negotiated sub-protocol remains `a2a.v1`.

### Added

- Native `wss://` (TLS) client support (spec Section 1), behind the new
  default `tls` feature:
  - Built on `rustls` with the `ring` crypto provider (no `aws-lc-rs` native
    build). TLS 1.3 and 1.2 are enabled.
  - Server certificates are validated against the platform trust store, with a
    fallback to the bundled Mozilla roots.
  - `TlsOptions` on `ConnectOptions` allows trusting additional PEM roots
    (`trust_pem`) for private CAs / self-signed certs, and a
    `danger_accept_invalid_certs` escape hatch for local testing only.
  - `parse_endpoint` now accepts `wss://` URLs (default port `443`). With the
    `tls` feature disabled, `wss://` connections return an error.
- Authentication and authorization support (spec Section 9):
  - `WsAuthenticator` trait for handshake authentication (`401`/`403`),
    per-message revalidation, server-initiated reauthentication
    (`ws/ReauthenticationRequired` + `4001` close), and optional in-band
    refresh via `ws/Authenticate`.
  - `websocket_router_with_auth` / `websocket_router_with_config` and
    `WebSocketConfig` (authenticator + Origin allowlist).
  - Client `ConnectOptions` for supplying handshake headers (e.g. bearer
    tokens) and `WebSocketTransport::authenticate` for in-band refresh.
  - Origin validation for Cross-Site WebSocket Hijacking mitigation
    (Section 9.7).

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
