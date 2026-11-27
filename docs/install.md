# Install

This page covers the two things you can install: the SDK crates, which you add to a Rust project, and the `a2acli` command-line client.

## Requirements

- Rust, installed with [`rustup`](https://rustup.rs). A stable toolchain is recommended. The crates declare 1.85 as their minimum version, and which crates build on that version differs:
    - `a2a-lf`, `a2a-pb`, `a2a-client-lf` and `a2a-server-lf` build on 1.85.
    - `a2a-grpc` and `a2a-slimrpc` need a newer compiler, because their dependencies do. 1.89 builds both.
    - To build the repository itself, use 1.89 or later. Its locked dependencies need at least 1.88.
- [CMake](https://cmake.org/download/), for `a2a-slimrpc` only. Its dependencies build a C library with it. The other crates do not need it, and `a2a-pb` bundles its own `protoc`.

## The SDK crates

The crates are published on [crates.io](https://crates.io). The core crates have an `-lf` suffix in their package names, and plain library names. `a2a-lf` is imported as `a2a`, so `use a2a::...` works.

Add what you need:

=== "Agent"

    ```bash
    cargo add a2a-lf a2a-server-lf
    ```

=== "Client"

    ```bash
    cargo add a2a-lf a2a-client-lf
    ```

=== "gRPC"

    ```bash
    cargo add a2a-lf a2a-grpc
    ```

=== "SLIM RPC"

    ```bash
    cargo add a2a-lf a2a-slimrpc
    ```

An agent also needs an async runtime, such as `tokio`, and `axum` to serve the routers. The [agent quickstart](quickstart/agent/index.md) shows a complete setup.

| Crate | Add it for |
|---|---|
| `a2a-lf` | The protocol types. Every other crate builds on it. |
| `a2a-server-lf` | An agent served over JSON-RPC and REST. |
| `a2a-client-lf` | A client that calls an agent over JSON-RPC and REST. |
| `a2a-grpc` | The gRPC binding, for a client and a server. |
| `a2a-slimrpc` | The SLIM RPC binding, for a client and a server. |
| `a2a-pb` | The protobuf messages and the conversions to and from the native types, when you need them directly. |

### TLS features

`a2a-client-lf`, `a2a-server-lf` and `a2a-grpc` each take a TLS feature, and `rustls-tls` is on by default. They share these names:

| Feature | What it does |
|---|---|
| `rustls-tls` | TLS with `rustls` and its default crypto provider, `aws-lc-rs`. The default. |
| `rustls-no-provider` | TLS with `rustls`, where your program installs the crypto provider. |
| `native-tls` | TLS with the platform's native stack. |

With `native-tls` alone, `a2a-server-lf` has no `tls` module, so it cannot serve HTTPS itself. The feature then covers push notification delivery.

To use a feature other than the default, turn the defaults off:

```bash
cargo add a2a-client-lf --no-default-features --features native-tls
```

If a crate depends on several of these, give them all the same feature. `a2a-grpc` enables the matching feature of `a2a-client-lf` and `a2a-server-lf` for you.

### The development version

To depend on the latest code on `main`, which may be ahead of the published crates, use Git dependencies:

```toml
[dependencies]
a2a = { package = "a2a-lf", git = "https://github.com/a2aproject/a2a-rs.git" }
a2a-server = { package = "a2a-server-lf", git = "https://github.com/a2aproject/a2a-rs.git" }
a2a-client = { package = "a2a-client-lf", git = "https://github.com/a2aproject/a2a-rs.git" }
```

## a2acli

`a2acli` is a command-line client for A2A agents. It is published as the `a2a-cli` package and installs the `a2acli` binary. Prebuilt binaries exist for macOS (arm64 and x86_64), Linux (x86_64 and aarch64) and Windows (x86_64).

=== "macOS"

    With [Homebrew](https://brew.sh):

    ```bash
    brew tap a2aproject/a2a-rs https://github.com/a2aproject/a2a-rs
    brew trust a2aproject/a2a-rs
    brew install a2acli
    ```

=== "Linux"

    With the install script:

    ```bash
    curl -fsSL https://raw.githubusercontent.com/a2aproject/a2a-rs/main/install.sh | bash
    ```

    The script installs to `/usr/local/bin` as root, and to `~/.local/bin` otherwise. Set `A2A_CLI_INSTALL_DIR` to choose another directory, and `A2A_CLI_VERSION` to install a specific version.

=== "Windows"

    With [WinGet](https://learn.microsoft.com/en-us/windows/package-manager/winget/):

    ```bash
    winget install a2aproject.a2acli-rs
    ```

=== "From source"

    With `cargo`, on any platform:

    ```bash
    cargo install a2a-cli
    ```

Check the install:

```bash
a2acli --version
```

The release archives are signed with [Sigstore](https://www.sigstore.dev/) and carry SLSA build provenance. The [`a2acli` README](https://github.com/a2aproject/a2a-rs/blob/main/a2acli/README.md#verifying-a-download) shows how to verify a download.

The [`a2acli` quickstart](quickstart/a2acli.md) runs the main commands, and the [command reference](reference/a2acli.md) covers the rest.

### The SLIM RPC plugin for the Go CLI

This is for the [Go A2A CLI](https://github.com/a2aproject/a2a-cli), not for `a2acli`. `a2a-transport-slimrpc` is a plugin that adds the SLIM RPC binding to it. The CLI finds the plugin on your `PATH` and launches it, so you install it separately. The [plugin README](https://github.com/a2aproject/a2a-rs/blob/main/a2acli-transport-slimrpc/README.md#install-a-prebuilt-binary) has the steps for macOS, Linux and Windows.

## Build from a checkout

To build the repository and run its tests, you need Rust 1.89 or later and CMake:

```bash
git clone https://github.com/a2aproject/a2a-rs.git
cd a2a-rs
cargo build
cargo test
```

Run from the root, `cargo` builds the default members, which are every crate except the interop test agent in `itk/`. That agent needs `protoc` installed, which is why `cargo build --workspace` fails without it.

The repository also has a [`justfile`](https://github.com/a2aproject/a2a-rs/blob/main/justfile) with the common commands, such as `just build`, `just test` and `just lint`. [`just`](https://github.com/casey/just) is optional.

## Next

- [Build an agent](quickstart/agent/index.md).
- [Call an agent from Rust](quickstart/client.md).
