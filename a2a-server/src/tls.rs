// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Re-exports for serving axum routers over HTTPS with rustls.
//!
//! This module is available when the `rustls` feature is enabled. It
//! re-exports compatible versions of [`rustls`] and [`axum_server`] so that
//! consumers do not need to manually align dependency versions.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use a2a_server::tls::{axum_server, rustls};
//!
//! # async fn example() {
//! let tls_config = rustls::ServerConfig::builder()
//!     .with_no_client_auth()
//!     .with_single_cert(vec![], rustls::pki_types::PrivateKeyDer::Pkcs8(vec![].into()))
//!     .unwrap();
//!
//! let app = axum::Router::new();
//! let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls_config));
//! let addr: std::net::SocketAddr = "0.0.0.0:3443".parse().unwrap();
//! axum_server::bind_rustls(addr, rustls_config)
//!     .serve(app.into_make_service())
//!     .await
//!     .unwrap();
//! # }
//! ```

pub use axum_server;
pub use rustls;
