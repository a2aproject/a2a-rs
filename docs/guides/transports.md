# Transports

An A2A agent can expose more than one binding. The protocol is the same on each;
what differs is the wire format and the crate that speaks it. This page covers
each binding the SDK ships, the crate and feature flag behind it, and how to
mount more than one on the same handler.

## Choosing a binding

| Binding | Crate | Wire format | When to use it |
| --- | --- | --- | --- |
| JSON-RPC | `a2a-server`, `a2a-client` | JSON over HTTP POST | Default. Widest client support and the simplest to debug. |
| REST | `a2a-server`, `a2a-client` | JSON over HTTP verbs | You want resource-oriented URLs and plain HTTP tooling. |
| gRPC | `a2a-grpc`, `a2a-pb` | Protocol Buffers | You already use gRPC, or want generated clients in many languages. |
| SLIMRPC | `a2a-slimrpc` | SLIM peer transport | You are inside a SLIM deployment and want its identity model. |

An agent advertises the bindings it serves in `supportedInterfaces`, one
`AgentInterface` per binding, so a client can pick one. Advertise only what you
actually mount: a client will trust the card.

## JSON-RPC and REST

Both are served by `a2a-server` and called by `a2a-client`. JSON-RPC answers on
one path; REST spreads the same operations across resource URLs. Mounting them
is one router call each, and both share the same `RequestHandler`:

```rust
--8<-- "examples/src/helloworld/server.rs:routers"
```

The agent card router is mounted alongside them, so `/.well-known/agent-card.json`
is served by the same process.

## gRPC

gRPC is served by `a2a-grpc`, which adapts a `RequestHandler` to the generated
service, and defined by the proto at `a2a-pb/proto/a2a.proto`. Build the service
from your handler:

```rust
--8<-- "examples/src/helloworld/server.rs:grpc"
```

Serve it with Tonic over a TCP listener. The hello world example serves HTTP and
gRPC from one process with `tokio::select!`; see
`examples/src/helloworld/server.rs` for the full `main`.

## SLIMRPC

`a2a-slimrpc` exposes an `a2a_server::RequestHandler` through `slim_rpc`, and
implements the A2A `Transport` so a card advertising `SLIMRPC` can be called over
a SLIM peer session; the server registers with `SlimRpcHandler::register(&slim_rpc::Server, ...)`.
The card's `supportedInterfaces[].url` is read as a SLIM peer
name, in the form `org/namespace/app` (or `slim://org/namespace/app`).

Unlike the bindings above, the crate has no runnable example in this repository
yet, so this page cannot include one from a compiled file. Its `README.md`
documents the caller-identity keys it sets on `ServiceParams` and the target
format in detail; `a2a-slimrpc/tests/e2e.rs` is the working reference.

## Serving several bindings from one handler

Every binding takes the same `RequestHandler`, so one executor can be reached
over JSON-RPC, REST and gRPC at once — which is what the hello world example
does. Build the handler once, hand a clone to each router and the gRPC service,
then run the listeners together. Nothing in the handler needs to know which
binding a request arrived on.

List each binding in the card's `supportedInterfaces` with the URL a client
should use, and keep those URLs in step with where you actually listen.

## TLS

`a2a_server::tls` re-exports the pieces needed to terminate TLS in-process. The
`rustls-tls` feature is on by default and uses `rustls`; `native-tls` switches
`a2a-server-lf` and `a2a-client-lf` to the platform TLS stack instead. Pick one and
enable it consistently on both sides:

```toml
# Default: rustls
a2a-server-lf = { version = "0.5", features = ["rustls-tls"] }

# Or the platform stack
a2a-server-lf = { version = "0.5", default-features = false, features = ["native-tls"] }
```

The server is built from a `rustls::ServerConfig` for HTTP and a second one for
gRPC, whose ALPN must advertise HTTP/2. The `helloworld_tls` example shows both:

```rust
--8<-- "examples/src/helloworld_tls/server.rs:tls-config"
```

Run it with `cargo run --bin helloworld-tls-server --package examples` and call
it with the matching client. The `helloworld_tls` example's `client.rs` shows the
other side: a `rustls::ClientConfig` with the CA added to the root store, passed
to `GrpcTransportFactory::with_rustls_config()`. (`TlsIncoming` is the
server-side accept-loop wrapper around `TcpIncoming`, not a client type.)

