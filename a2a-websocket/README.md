# a2a-websocket

WebSocket custom protocol binding for A2A v1 client and server implementations.
The binding transports JSON-RPC 2.0 messages over a persistent WebSocket
connection.

This crate is published as `a2a-websocket` and imported in Rust as
`a2a_websocket`.

## What It Provides

- `Transport` implementation for A2A over a single multiplexed WebSocket
  connection (`WebSocketTransport`).
- `TransportFactory` integration for agent cards that advertise
  `WEBSOCKET` (`WebSocketTransportFactory`).
- An `axum::Router` builder (`websocket_router`, plus
  `websocket_router_with_auth` / `websocket_router_with_config`) that adapts an
  `a2a_server::RequestHandler` to serve A2A operations over a persistent
  WebSocket connection with bidirectional streaming.
- A pluggable authentication layer (`WsAuthenticator`) covering handshake
  authentication, per-message revalidation, server-initiated reauthentication,
  and optional in-band token refresh — with client-side `ReauthPolicy` support
  for reacting to a reauthentication request.
- Inbound message rate limiting (`RateLimit`) applied per connection and per
  authenticated identity, plus an inbound message size cap.
- Keep-alive pings and idle timeouts (`Liveness`), and caps on connections per
  identity and streams per connection (`ConnectionLimits`).
- Reconnection backoff with jitter (`Backoff`,
  `WebSocketTransport::connect_with_retry`).
- Mapping between `a2a::A2AError` and JSON-RPC 2.0 error objects (numeric
  `code` + structured `data`), including close-code selection for fatal
  failures.

## Connection Model

Each connection is genuinely full duplex. Requests are correlated by JSON-RPC
`id`, so a server can push stream chunks while the client sends new requests over
the same socket, and a `cancelStream` arriving mid-stream is acted on
immediately.

Internally, each connection runs two tasks: one owns the read half of the socket
and one owns the write half, with every outbound frame — responses, stream
chunks, and the control frames owed to the peer such as pongs and close echoes —
queued through a channel to the writer. Driving both directions from a single
task instead would mean racing a read against a write and discarding whichever
loses, and `fastwebsockets::read_frame` cannot be cancelled safely: it consumes
frame headers from a persistent buffer into local variables, so abandoning it
partway through desynchronizes the parser. Splitting removes that hazard, and it
also keeps a large write from stalling reads.

## Wire Format

The wire format follows the A2A **WebSocket** custom protocol binding
specification, which transports JSON-RPC 2.0 messages:

- Sub-protocol: `a2a.v1` (negotiated via `Sec-WebSocket-Protocol`).
- Every message is a UTF-8 **JSON-RPC 2.0** object (`"jsonrpc": "2.0"`) carried
  in a text frame.
- Requests use `id` (string or number), `method`, and `params`; the binding
  adds the extension members `serviceParams` (per-request metadata) and
  `cancelStream`.
- Unary responses and individual streaming chunks are both carried in the
  `result` member. A stream is terminated by a `streamEnd: true` sentinel;
  clients cancel an in-progress stream by sending a `cancelStream: true`
  envelope carrying the original request `id`.
- Errors use the standard JSON-RPC `error` object; the numeric `code` is the
  canonical A2A error code and `error.data` carries the structured `ErrorInfo`
  detail.

## Authentication

Credentials are presented once during the HTTP Upgrade handshake and apply for
the lifetime of the connection (spec Section 9). Provide an implementation of
`WsAuthenticator` and register it with `websocket_router_with_auth`:

```rust,ignore
use std::sync::Arc;
use async_trait::async_trait;
use axum::http::HeaderMap;
use a2a_websocket::auth::{AuthContext, AuthError, User, WsAuthenticator};
use a2a_websocket::server::websocket_router_with_auth;

struct BearerAuth;

#[async_trait]
impl WsAuthenticator for BearerAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some(token) if validate(token) => Ok(AuthContext::for_user(User::authenticated("id"))),
            Some(_) => Err(AuthError::forbidden("invalid token")),
            None => Err(AuthError::unauthorized("missing Authorization header")),
        }
    }
}

let app = axum::Router::new()
    .nest("/a2a/ws", websocket_router_with_auth(handler, Arc::new(BearerAuth)));
```

- **Handshake auth (Section 9.1):** returning `AuthError` rejects the upgrade
  with `401`/`403` — the connection is never established.
- **Revalidation (Section 9.3.1):** override `revalidate` to re-check
  credentials before each request. Returning `AuthStatus::Expired` closes the
  connection with code `4001` after sending a final error (`-32000`).
- **Server-initiated reauth (Section 9.3.2):** returning
  `AuthStatus::ReauthRequired` emits a `ReauthenticationRequired` control frame
  and closes with `4001` after a grace period.
- **In-band refresh (Section 9.3.3):** set `supports_in_band_refresh` to `true`
  and implement `refresh` to accept the binding-specific `Authenticate` method.
  Clients call `WebSocketTransport::authenticate(scheme, credentials)`. When
  supported, this MUST also be advertised in the agent's binding-specific
  capabilities.

Clients supply handshake credentials via `ConnectOptions`:

```rust,ignore
use a2a_websocket::{ConnectOptions, WebSocketTransport};

let transport = WebSocketTransport::connect_with_options(
    "ws://127.0.0.1:9000/a2a/ws",
    ConnectOptions::with_bearer_token("my-token"),
).await?;
```

Section 9.3.2 requires clients to *handle* a `ReauthenticationRequired` control
frame, so any connection whose credentials can expire should set a
`ReauthPolicy`. Either be notified and reconnect yourself:

```rust,ignore
let options = ConnectOptions::with_bearer_token("my-token")
    .on_reauth_required(|request| {
        tracing::info!(reason = ?request.reason, "reconnect with fresh credentials");
    });
```

…or supply a `CredentialProvider` and let the transport refresh in-band
(Section 9.3.3), which keeps the connection open:

```rust,ignore
use a2a_websocket::{AuthenticateParams, CredentialProvider};

struct Tokens;

#[async_trait]
impl CredentialProvider for Tokens {
    async fn fresh_credentials(&self) -> Result<AuthenticateParams, A2AError> {
        Ok(AuthenticateParams { scheme: "Bearer".into(), credentials: mint_token().await? })
    }
}

let options = ConnectOptions::with_bearer_token("my-token")
    .with_credential_provider(Arc::new(Tokens));
```

Without a policy the frame is only logged, and the server closes the connection
with `4001` once its grace period expires. Close codes are surfaced to callers,
so a request failing after a `4001`, `1008`, `1009`, or `4002` close reports that
reason rather than a generic disconnect.

### Getting the identity to your agent

`AuthContext` has two fields, and the distinction matters:

- `user` is consumed **by the binding**. It scopes the per-identity rate-limit
  budget; it is not passed to the `RequestHandler`.
- `service_params` is the seam **to your agent**. Every entry is merged into the
  `ServiceParams` of each request on the connection, which is what the handler
  and, through it, your `AgentExecutor` receive as
  `ExecutorContext::service_params`.

So publish whatever your business logic needs to act on:

```rust,ignore
Ok(AuthContext::for_user(User::authenticated(&subject))
    .with_param("x-agent-user", &subject)
    .with_param("x-tenant", &tenant))
```

These keys are lowercased and are authoritative for the connection: a
per-request `serviceParams` entry of the same name is ignored, so a client
cannot present itself to the handler as a principal it did not authenticate as.
Keys the authenticator does not set stay client-settable per request. An in-band
refresh replaces them, so a connection never keeps acting on the authorization
it held before the refresh.

## Rate Limiting

Servers are required to rate limit inbound messages (Section 13.3), so the limit
is **on by default**: every router enforces `DEFAULT_RATE_LIMIT` (100 messages
per second) unless told otherwise. The budget is enforced per connection *and*
across all connections of the same authenticated identity, so opening more
sockets does not buy more throughput.

```rust,ignore
use std::time::Duration;
use a2a_websocket::{RateLimit, RateLimitPolicy, WebSocketConfig, server::websocket_router_with_config};

let config = WebSocketConfig {
    rate_limit: RateLimitPolicy::Custom(RateLimit::new(500, Duration::from_secs(1))),
    ..Default::default()
};
let app = axum::Router::new()
    .nest("/a2a/ws", websocket_router_with_config(handler, config));
```

`max_messages` is also the burst capacity: a connection may send that many
messages immediately, then settles at `max_messages` per `window`. Exceeding the
budget returns a `-32000` error with message `"Rate limit exceeded"` and closes
with `1008`.

`RateLimitPolicy::Disabled` switches the limit off. Because that opts out of a
MUST, it is only appropriate when a trusted layer in front of the agent enforces
its own.

Origin validation (Cross-Site WebSocket Hijacking mitigation, Section 13.2) can
be enabled with `WebSocketConfig::allowed_origins`.

Inbound messages larger than `WebSocketConfig::max_frame_bytes` (1 MiB by
default, Section 3.6) are rejected with a `1009` close.

## Keep-Alive and Idle Timeouts

Servers ping on an interval and close connections that stop answering or stop
being used (Section 2.4). This is **on by default** with the intervals the spec
recommends — ping every 30s, expect a pong within 10s, close after 5 minutes with
no application-level message:

```rust,ignore
use std::time::Duration;
use a2a_websocket::{Liveness, LivenessPolicy, WebSocketConfig};

let config = WebSocketConfig {
    liveness: LivenessPolicy::Custom(Liveness {
        ping_interval: Duration::from_secs(15),
        pong_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(120),
    }),
    ..Default::default()
};
```

Both timeouts close with `1001` (Going Away). Traffic in *either* direction
counts as activity, so a long-running stream pushing events to a quiet client is
never mistaken for idle. `LivenessPolicy::Disabled` switches this off, which
suits deployments where a proxy already handles it.

## Connection and Stream Limits

Servers cap concurrent connections per authenticated identity and concurrent
streams per connection (Section 13.5). Both are **on by default** with
deliberately generous values (256 and 128), since the spec recommends no
figures — they exist to stop one identity exhausting a server, not to shape
normal traffic:

```rust,ignore
use a2a_websocket::{ConnectionLimitPolicy, ConnectionLimits, WebSocketConfig};

let config = WebSocketConfig {
    connection_limits: ConnectionLimitPolicy::Custom(ConnectionLimits {
        max_connections_per_identity: 4,
        max_streams_per_connection: 16,
    }),
    ..Default::default()
};
```

An identity at its connection limit is refused during the upgrade with `503`, so
a client can back off and return rather than being handed a connection that
closes immediately. A request that would exceed the stream cap gets a `-32000`
error with message `"Too many concurrent streams"`; the connection and its
existing streams are unaffected. Anonymous connections are not counted against
any identity, so set `allowed_origins` or an authenticator if you rely on this.

## Reconnection

`connect_with_retry` re-establishes a connection on an exponential backoff with
jitter, defaulting to the schedule Section 8.4 recommends (1s, doubling, capped
at 30s):

```rust,ignore
use a2a_websocket::{Backoff, ConnectOptions, WebSocketTransport};

let transport = WebSocketTransport::connect_with_retry(
    "wss://agent.example.com/a2a/ws",
    ConnectOptions::with_bearer_token(token),
    Backoff::default(),
)
.await?;
```

Failures that would recur are **not** retried — a rejected token, an endpoint
that is not a WebSocket server, or a sub-protocol the server will not speak fail
immediately, so a reconnect loop cannot end up hammering an auth server. `429`
and `5xx` are retried, which covers the `503` returned by a per-identity
connection cap.

Resubscribing after a reconnect is left to you: issue `SubscribeToTask` for the
task ids you still care about, and be ready for events you have already seen
(Section 8.4 steps 2 and 3). The transport does not do this itself, because only
the application knows which tasks still matter and whether replaying an event is
safe.

## Agent Card Endpoint Format

The existing `AgentInterface` model only carries a string target, so the
WebSocket binding interprets `supportedInterfaces[].url` as an absolute
WebSocket endpoint. Accepted forms are:

- `wss://host:port[/path]` (TLS)
- `ws://host:port[/path]`
- `host:port[/path]` (normalized to `ws://`)

The transport identifier in agent cards is `WEBSOCKET`, exposed as the
`a2a::TRANSPORT_PROTOCOL_WEBSOCKET` constant.

## TLS (`wss://`)

Native `wss://` client support is provided by the default `tls` feature, built
on [`rustls`] with the `ring` crypto provider. The client validates the server
certificate against the platform trust store (falling back to the bundled
Mozilla roots). Private CAs and self-signed certificates can be trusted
explicitly via `TlsOptions`:

```rust,ignore
use a2a_websocket::{ConnectOptions, TlsOptions, WebSocketTransport};

// Trust a private CA / self-signed cert (PEM).
let options = ConnectOptions::default()
    .with_tls(TlsOptions::default().trust_pem(std::fs::read("ca.pem")?));

let transport = WebSocketTransport::connect_with_options(
    "wss://agent.example.com/a2a/ws",
    options,
).await?;
```

`TlsOptions::danger_accept_invalid_certs()` disables certificate verification.
It exists for local testing only and must never be used in production, as it
removes protection against man-in-the-middle attacks.

Disable the `tls` feature (`default-features = false`) if you terminate TLS at a
reverse proxy in front of the agent and only need `ws://`; `wss://` endpoints
then return an error.

The server side does not terminate TLS itself: serve `websocket_router(...)`
behind a TLS-terminating listener (e.g. `axum-server`/`tokio-rustls` or a
reverse proxy). The WebSocket upgrade works transparently over the terminated
connection.

[`rustls`]: https://docs.rs/rustls

## Example: server

```rust,ignore
use std::sync::Arc;
use a2a_server::handler::DefaultRequestHandler;
use a2a_server::task_store::InMemoryTaskStore;
use a2a_websocket::server::websocket_router;

let handler = Arc::new(DefaultRequestHandler::new(my_executor, InMemoryTaskStore::new()));
let app = axum::Router::new().nest("/a2a/ws", websocket_router(handler));
```

## Example: client

```rust,ignore
use a2a_client::transport::{ServiceParams, Transport};
use a2a_websocket::WebSocketTransport;

let transport = WebSocketTransport::connect("ws://127.0.0.1:9000/a2a/ws").await?;
let response = transport.send_message(&ServiceParams::new(), &request).await?;
```

## Runnable Example

A complete application — authenticator, rate limit, identity-aware executor, and
a client covering both reauthentication policies — lives in the workspace
`examples` crate:

```bash
cargo run --bin websocket-server --package examples
cargo run --bin websocket-client --package examples
```

## Install

```toml
[dependencies]
a2a = { package = "a2a-lf", version = "0.2" }
a2a-websocket = { package = "a2a-websocket", version = "0.1" }
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
