// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("config file not found: A2A_SLIMRPC_PLUGIN_CONFIG env var is not set")]
    ConfigEnvMissing,

    #[error("failed to read config file '{path}': {source}")]
    ConfigRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config file '{path}': {source}")]
    ConfigParse {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("auth config error: {0}")]
    Auth(#[from] slim_config::auth::ConfigAuthError),

    #[error("SLIM service error: {0}")]
    Slim(String),

    #[error("TLS setup error: {0}")]
    Tls(String),

    #[error("server bind error: {0}")]
    Bind(#[from] std::io::Error),

    #[error("handshake write error: {0}")]
    Handshake(String),

    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),

    #[error("unsupported binding '{0}': expected grpc, jsonrpc, or http+json")]
    UnsupportedBinding(String),
}
