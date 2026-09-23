// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
pub mod card_schema;

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use a2a::*;
use a2a_client::auth::AuthInterceptor;
use a2a_client::{A2AClient, A2AClientFactory, BoxStream};
use clap::parser::ValueSource;
use clap::{ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use futures::StreamExt;
use reqwest::{Client, RequestBuilder};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(
    name = "a2acli",
    version,
    about = "Standalone A2A client CLI",
    after_help = "\
Default behavior (SPEC.md §6.5):
  - Transport: the agent card's first supported interface (--transport overrides)
  - Task completion: wait until terminal/interrupted state (--async returns immediately)
  - Output: human-readable text (-o/--output json switches to protocol JSON)
  - Protocol version: highest shared 1.x, from the agent card (--a2a-version pins it)
  - Transport security: TLS verification on (--insecure disables it, with a warning)"
)]
pub struct Cli {
    /// Load configuration from an explicit .env file in place of the local
    /// .env (found by walking up from the working directory). Real
    /// environment variables still take precedence over it (§6.5).
    #[arg(long, global = true)]
    pub config: Option<String>,

    /// The agent to talk to, as an Agent Card reference (§7.2): a bare host
    /// or origin (the well-known path is appended), a full card URL (used
    /// as-is), or a local file path (`file://…` or a plain path).
    #[arg(
        short = 'a',
        long = "agent-card",
        global = true,
        value_name = "REF",
        env = "A2ACLI_AGENT_CARD"
    )]
    pub agent_card: Option<String>,

    /// Connect straight to an agent interface URL, skipping Agent Card
    /// resolution (§7.2). MUST be used with exactly one --transport, since
    /// there is no card to name the binding. Mutually exclusive with
    /// --agent-card.
    #[arg(
        short = 'e',
        long,
        global = true,
        value_name = "REF",
        env = "A2ACLI_ENDPOINT"
    )]
    pub endpoint: Option<String>,

    /// Deprecated alias for the bare-origin form of --agent-card, kept so
    /// pre-#178 invocations keep working. Hidden: `--agent-card` is the
    /// flag the specification defines.
    #[arg(
        long,
        global = true,
        hide = true,
        default_value = "http://localhost:3000",
        env = "A2ACLI_BASE_URL"
    )]
    pub base_url: String,

    /// Client transport preference, repeatable and ordered (highest first):
    /// e.g. `--transport jsonrpc --transport rest`. Overrides the agent
    /// card's own preference order; a binding the card doesn't offer is
    /// skipped.
    #[arg(
        long,
        global = true,
        value_enum,
        env = "A2ACLI_TRANSPORT",
        value_delimiter = ','
    )]
    pub transport: Vec<Binding>,

    /// Bearer token attached to the agent-card fetch and client calls.
    /// Protocol version to signal to the server on every request (§13.2).
    /// Absent this flag the version is negotiated down to the highest one
    /// both a2acli and the agent's selected interface declare. Must be
    /// 1.x: A2A reads an empty or pre-1.0 value as 0.3.
    #[arg(long = "a2a-version", global = true, env = "A2ACLI_A2A_VERSION")]
    pub a2a_version: Option<String>,

    #[arg(long, global = true, env = "A2ACLI_BEARER")]
    pub bearer: Option<String>,

    /// API key attached to the agent-card fetch and client calls (as an
    /// `X-API-Key` header — a2acli does not yet read the agent card's
    /// declared security scheme to place it elsewhere per A2A §4.5.2).
    #[arg(long = "api-key", global = true, env = "A2ACLI_API_KEY")]
    pub api_key: Option<String>,

    /// Add an A2A service parameter (a transport-level key-value pair, e.g.
    /// a header or gRPC metadata) — general-purpose, not authentication-
    /// specific. Distinct from `--metadata`, which travels in the request
    /// payload.
    #[arg(long = "svc-param", global = true, value_parser = parse_header)]
    pub svc_params: Vec<HeaderArg>,

    /// Disable TLS certificate verification for the negotiated transport.
    /// Development only — always prints a warning, and never disables
    /// verification silently.
    #[arg(long, global = true, env = "A2ACLI_INSECURE")]
    pub insecure: bool,

    /// Verbose diagnostics to stderr: the outcome of each call plus the raw
    /// protocol messages exchanged on the wire — request and response
    /// bodies, and each streamed event (§7.2). Never includes credential
    /// material (bearer token, API key, or any --svc-param value): header
    /// names stay visible so their attachment can be confirmed, but values
    /// are replaced, and that redaction cannot be defeated by this or any
    /// other verbosity flag.
    #[arg(long, global = true, env = "A2ACLI_DEBUG")]
    pub debug: bool,

    /// Optional tenant forwarded to A2A requests that support it. Overrides
    /// the routing tenant the selected Agent Card interface may itself
    /// declare (A2A §8.3.2); omit this to use the interface's own value,
    /// if it has one.
    #[arg(long, global = true, env = "A2ACLI_TENANT")]
    pub tenant: Option<String>,

    /// Output format: human-readable `text` (default) or the protocol's own
    /// `json` types. Cardinality (one document vs. JSONL) follows `--stream`,
    /// not this flag (§11.3).
    #[arg(
        short = 'o',
        long,
        global = true,
        value_enum,
        default_value_t = OutputFormat::Text,
        env = "A2ACLI_OUTPUT"
    )]
    pub output: OutputFormat,

    /// With `-o json` (and no `--stream`), emit compact JSON instead of
    /// pretty-printed JSON. Has no effect on `-o text` or on `--stream`
    /// JSONL, which is always one compact object per line regardless of
    /// this flag (§11.3).
    #[arg(long, global = true, env = "A2ACLI_COMPACT")]
    pub compact: bool,

    /// Do not wait: return the task identifiers immediately instead of blocking
    /// until the task reaches a terminal or interrupted state (the default).
    ///
    /// Note: the spec lists `--return-immediately` as an OPTIONAL alias for
    /// this flag, but that name is already this CLI's flag for the distinct
    /// protocol-level `SendMessageConfiguration.return_immediately` (a
    /// request to the *agent* to return early on queued work, not a
    /// client-side "don't poll" instruction) — so it is deliberately not
    /// reused here to avoid conflating the two.
    #[arg(
        long = "async",
        alias = "no-wait",
        global = true,
        conflicts_with = "wait",
        env = "A2ACLI_ASYNC"
    )]
    pub async_mode: bool,

    /// Explicitly block until the task reaches a terminal or interrupted state.
    /// This is already the default for `send`; on `task get` it turns the
    /// one-shot read into a poll loop.
    #[arg(
        long,
        global = true,
        conflicts_with = "async_mode",
        env = "A2ACLI_WAIT"
    )]
    pub wait: bool,

    /// Delay between polls while waiting for a task to settle (e.g. "2s", "500ms").
    #[arg(long, global = true, value_parser = parse_duration, default_value = "2s", env = "A2ACLI_POLL_INTERVAL")]
    pub poll_interval: Duration,

    /// Overall time budget for a blocking wait before reporting a timeout (e.g. "30s", "2m").
    #[arg(long, global = true, value_parser = parse_duration, default_value = "30s", env = "A2ACLI_TIMEOUT")]
    pub timeout: Duration,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Agent Card operations.
    Card {
        #[command(subcommand)]
        command: CardCommand,
    },
    /// Configuration inspection.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Send a message to start or continue an interaction.
    Send(MessageCommand),
    /// Task operations.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Emit a shell completion script for the named shell on stdout (§7.1).
    Completion {
        /// Shell to emit the script for. An unrecognised name is a usage
        /// error naming the accepted values.
        #[arg(value_name = "SHELL")]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum CardCommand {
    /// Fetch and print the Agent Card. Pass --extended for the authenticated extended card.
    Get(CardGetCommand),
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum ConfigCommand {
    /// Print each effective setting and the source it resolved from
    /// (flag, environment variable, local .env file, global .env file, or
    /// built-in default). Read-only: credential values are redacted, and
    /// this command never edits configuration itself (§8.3).
    Show,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct CardGetCommand {
    /// Fetch the authenticated extended agent card instead of the public one.
    #[arg(long)]
    pub extended: bool,

    /// Validate the fetched card against the A2A JSON schema (§10.1) and
    /// report every violation, rather than only the type check
    /// deserialization already performs on every `card get`.
    #[arg(long)]
    pub validate: bool,
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum TaskCommand {
    /// Fetch a task by ID.
    Get(TaskLookupCommand),
    /// List tasks with optional filters.
    List(ListTasksCommand),
    /// Cancel a task by ID.
    Cancel(TaskIdCommand),
    /// Subscribe to task updates and print each event as it arrives.
    Subscribe(TaskIdCommand),
    /// Manage push notification configs for a task.
    PushConfig {
        #[command(subcommand)]
        command: PushConfigCommand,
    },
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct MessageCommand {
    /// Text payload to send as the user message. Shorthand for a single
    /// --text-part when no other part flags are given; mutually exclusive
    /// with --text-part/--file-part/--data-part.
    pub text: Option<String>,

    /// Add a text part. Repeatable and order-preserving alongside
    /// --file-part/--data-part.
    #[arg(long = "text-part")]
    pub text_parts: Vec<String>,

    /// Add a file part. A local path is inlined as bytes; a URL is carried
    /// by reference and never fetched by the CLI. Repeatable.
    #[arg(long = "file-part")]
    pub file_parts: Vec<String>,

    /// Add a structured JSON data part, read from a file path, parsed from
    /// an inline JSON string, or "-" to read JSON from stdin. Repeatable.
    #[arg(long = "data-part")]
    pub data_parts: Vec<String>,

    /// Media type for the part flag immediately preceding it (--file-part or
    /// --data-part). Usage error if it follows no part flag.
    #[arg(long = "media-type")]
    pub media_types: Vec<String>,

    /// Optional context identifier to continue an existing conversation.
    #[arg(long, env = "A2ACLI_CONTEXT_ID")]
    pub context_id: Option<String>,

    /// Optional task identifier to continue an existing task.
    #[arg(long, env = "A2ACLI_TASK_ID")]
    pub task_id: Option<String>,

    /// Ask the server to include up to this many history items in task responses.
    #[arg(long)]
    pub history_length: Option<i32>,

    /// Accepted output mode, for example text/plain or application/json.
    #[arg(long = "accept-output")]
    pub accepted_output_modes: Vec<String>,

    /// Ask the server to return immediately when it supports queued work.
    #[arg(long)]
    pub return_immediately: bool,

    /// Use the streaming send operation and print each event as it arrives.
    #[arg(long)]
    pub stream: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct TaskLookupCommand {
    /// Task identifier.
    pub id: String,

    /// Ask the server to include up to this many history items.
    #[arg(long)]
    pub history_length: Option<i32>,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct ListTasksCommand {
    /// Filter by context identifier.
    #[arg(long)]
    pub context_id: Option<String>,

    /// Filter by task state.
    #[arg(long, value_enum)]
    pub status: Option<TaskStateArg>,

    /// Requested page size.
    #[arg(long)]
    pub page_size: Option<i32>,

    /// Page token from a previous response.
    #[arg(long)]
    pub page_token: Option<String>,

    /// Ask the server to include up to this many history items per task.
    #[arg(long)]
    pub history_length: Option<i32>,

    /// Ask the server to include artifacts in the listed tasks.
    #[arg(long)]
    pub include_artifacts: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct TaskIdCommand {
    /// Task identifier.
    pub id: String,
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum PushConfigCommand {
    /// Create a push notification config for a task.
    Create(CreatePushConfigCommand),
    /// Fetch a push notification config by ID.
    Get(PushConfigIdCommand),
    /// List push notification configs for a task.
    List(ListPushConfigsCommand),
    /// Delete a push notification config by ID.
    Delete(PushConfigIdCommand),
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct CreatePushConfigCommand {
    /// Task identifier.
    pub task_id: String,

    /// Callback URL that will receive push notifications.
    pub url: String,

    /// Optional push config identifier.
    #[arg(long = "config-id")]
    pub config_id: Option<String>,

    /// Optional push notification token.
    #[arg(long)]
    pub token: Option<String>,

    /// Optional authentication scheme, for example Bearer.
    #[arg(long = "auth-scheme")]
    pub auth_scheme: Option<String>,

    /// Optional authentication credentials.
    #[arg(long = "auth-credentials")]
    pub auth_credentials: Option<String>,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct PushConfigIdCommand {
    /// Task identifier.
    pub task_id: String,

    /// Push config identifier.
    pub id: String,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct ListPushConfigsCommand {
    /// Task identifier.
    pub task_id: String,

    /// Requested page size.
    #[arg(long)]
    pub page_size: Option<i32>,

    /// Page token from a previous response.
    #[arg(long)]
    pub page_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderArg {
    pub name: String,
    pub value: String,
}

/// Output format (§6.5, §11.2, §11.3). `Text` is the default, human-readable
/// floor; `Json` emits the protocol's own response types, one document or
/// JSONL depending on `--stream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Binding {
    Jsonrpc,
    /// The A2A HTTP+JSON binding — spelled `rest` on the command line to
    /// match the reference implementation's `--transport` vocabulary.
    #[value(name = "rest")]
    Rest,
}

impl Binding {
    fn protocol(self) -> &'static str {
        match self {
            Binding::Jsonrpc => TRANSPORT_PROTOCOL_JSONRPC,
            Binding::Rest => TRANSPORT_PROTOCOL_HTTP_JSON,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TaskStateArg {
    Unspecified,
    Submitted,
    Working,
    Completed,
    Failed,
    Canceled,
    InputRequired,
    Rejected,
    AuthRequired,
}

impl From<TaskStateArg> for TaskState {
    fn from(value: TaskStateArg) -> Self {
        match value {
            TaskStateArg::Unspecified => TaskState::Unspecified,
            TaskStateArg::Submitted => TaskState::Submitted,
            TaskStateArg::Working => TaskState::Working,
            TaskStateArg::Completed => TaskState::Completed,
            TaskStateArg::Failed => TaskState::Failed,
            TaskStateArg::Canceled => TaskState::Canceled,
            TaskStateArg::InputRequired => TaskState::InputRequired,
            TaskStateArg::Rejected => TaskState::Rejected,
            TaskStateArg::AuthRequired => TaskState::AuthRequired,
        }
    }
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error(transparent)]
    A2A(#[from] A2AError),
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("failed to serialize output: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("failed to read {path}: {source}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("timed out after {timeout:?} waiting for task {task_id} to settle")]
    Timeout { task_id: String, timeout: Duration },
    /// A local Agent Card file that couldn't be read — the file-path
    /// counterpart of a non-2xx card fetch.
    #[error("failed to read agent card file {path}: {source}")]
    CardFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// A local Agent Card file that was read but isn't an Agent Card — the
    /// counterpart of a response body that won't deserialize.
    #[error("agent card file is not a valid agent card: {0}")]
    CardInvalid(String),
    /// `card get --validate`: the card deserialized fine (so `CardInvalid`
    /// does not apply) but fails the A2A JSON schema (§10.1) — distinct
    /// from a type-check failure, and carrying every violation rather than
    /// only the first.
    #[error("agent card failed schema validation ({} violation(s))", violations.len())]
    CardSchemaInvalid {
        violations: Vec<card_schema::Violation>,
    },
    /// A malformed invocation, as clap describes it. §11.4 counts a
    /// malformed flag among the CLI-local failures that must still be
    /// machine-readable, so clap's prose becomes the envelope's message
    /// rather than being printed in its place.
    #[error("{0}")]
    Usage(String),
}

/// A2A §5.3: where an agent publishes its card, appended to a reference
/// that names only a host or origin.
const WELL_KNOWN_AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// The Appendix B error envelope: the one result shape this specification
/// defines of its own, for a failure that never reached the protocol.
#[derive(Debug, Clone, Serialize)]
struct ErrorEnvelope {
    error: ErrorDetail,
}

#[derive(Debug, Clone, Serialize)]
struct ErrorDetail {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    #[serde(rename = "a2aCode", skip_serializing_if = "Option::is_none")]
    a2a_code: Option<i64>,
    /// Every schema violation on `CardSchemaInvalid`; absent otherwise. Its
    /// own field rather than folded into `message`, so `-o json` can walk
    /// each violation's path without parsing prose.
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Vec<card_schema::Violation>>,
}

impl CliError {
    /// The exit status this failure maps to (§11.6, Appendix D). `0` is
    /// never returned here — success never constructs a `CliError`.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::A2A(_) => 1,
            CliError::Http(error) => http_error_exit_code(error),
            CliError::Json(_) => 1,
            CliError::InvalidInput(_) => 2,
            CliError::ReadFile { .. } => 2,
            CliError::Timeout { .. } => 5,
            // Appendix D: the same statuses the HTTP card path uses, so a
            // caller branching on the exit code needn't know whether the
            // card came from a URL or a file.
            CliError::CardFile { .. } => 3,
            CliError::CardInvalid(_) => 1,
            CliError::CardSchemaInvalid { .. } => 1,
            CliError::Usage(_) => 2,
        }
    }

    /// The Appendix B error envelope for this failure: a protocol failure
    /// carries the A2A error name unchanged (§11.4); a CLI-local failure
    /// carries an `A2ACLI_ERR_*` symbol from Appendix D.
    fn envelope(&self) -> ErrorEnvelope {
        let (code, hint, a2a_code) = match self {
            CliError::A2A(error) => (
                error_reason(error.code).to_string(),
                None,
                Some(error.code as i64),
            ),
            CliError::Http(error) => http_error_code_and_hint(error),
            CliError::Json(_) => ("A2ACLI_ERR_INTERNAL".to_string(), None, None),
            CliError::InvalidInput(_) => ("A2ACLI_ERR_USAGE".to_string(), None, None),
            CliError::ReadFile { .. } => (
                "A2ACLI_ERR_USAGE".to_string(),
                Some(
                    "check that the --file-part/--data-part path exists and is readable"
                        .to_string(),
                ),
                None,
            ),
            CliError::CardFile { .. } => (
                "A2ACLI_ERR_CARD_NOT_FOUND".to_string(),
                Some("check that the --agent-card path exists and is readable".to_string()),
                None,
            ),
            CliError::Usage(_) => (
                "A2ACLI_ERR_USAGE".to_string(),
                Some("run `a2acli --help`, or `a2acli <command> --help`".to_string()),
                None,
            ),
            CliError::CardInvalid(_) => (
                "A2ACLI_ERR_CARD_INVALID".to_string(),
                Some("the agent card did not match the expected schema".to_string()),
                None,
            ),
            CliError::CardSchemaInvalid { .. } => (
                "A2ACLI_ERR_CARD_INVALID".to_string(),
                Some(format!(
                    "against the A2A JSON schema, version {}; see `details`",
                    card_schema::SCHEMA_A2A_VERSION
                )),
                None,
            ),
            CliError::Timeout { .. } => (
                "A2ACLI_ERR_TIMEOUT".to_string(),
                Some(
                    "increase --timeout/--poll-interval, or check the task later with `task get <id> --wait`"
                        .to_string(),
                ),
                None,
            ),
        };

        let details = match self {
            CliError::CardSchemaInvalid { violations } => Some(violations.clone()),
            _ => None,
        };

        ErrorEnvelope {
            error: ErrorDetail {
                code,
                message: self.to_string(),
                hint,
                a2a_code,
                details,
            },
        }
    }

    /// Print the Appendix B error envelope to stderr as one compact JSON
    /// line, in every output mode: §11.4 requires errors to be
    /// machine-readable unconditionally, not only under `-o json`.
    pub fn report(&self) {
        match serde_json::to_string(&self.envelope()) {
            Ok(json) => eprintln!("{json}"),
            Err(_) => eprintln!(
                "{{\"error\":{{\"code\":\"A2ACLI_ERR_INTERNAL\",\"message\":{:?}}}}}",
                self.to_string()
            ),
        }
    }
}

/// `reqwest::Error` from resolving the Agent Card (the only place it's
/// produced) classified against Appendix D: a connection/DNS/TLS failure is
/// `UNREACHABLE`; a non-2xx response is `CARD_NOT_FOUND` (or `AUTH_FAILED`
/// for 401/403); a body that failed to deserialize as an `AgentCard` is
/// `CARD_INVALID`.
fn http_error_code_and_hint(error: &reqwest::Error) -> (String, Option<String>, Option<i64>) {
    if error.is_decode() {
        return (
            "A2ACLI_ERR_CARD_INVALID".to_string(),
            Some("the agent card did not match the expected schema".to_string()),
            None,
        );
    }

    if let Some(status) = error.status() {
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return (
                "A2ACLI_ERR_AUTH_FAILED".to_string(),
                Some("credentials were rejected fetching the agent card".to_string()),
                None,
            );
        }
        return (
            "A2ACLI_ERR_CARD_NOT_FOUND".to_string(),
            Some("check --agent-card/--base-url resolves to a valid agent".to_string()),
            None,
        );
    }

    (
        "A2ACLI_ERR_UNREACHABLE".to_string(),
        Some("check --agent-card/--endpoint and network connectivity".to_string()),
        None,
    )
}

fn http_error_exit_code(error: &reqwest::Error) -> i32 {
    if error.is_decode() {
        return 1;
    }
    if let Some(status) = error.status() {
        return if status.as_u16() == 401 || status.as_u16() == 403 {
            4
        } else {
            3
        };
    }
    3
}

/// Parse `args` the same way the real binary parses `std::env::args_os()`, then
/// run the resulting command. Kept separate from [`run`] because recovering
/// the interleaved order of `send`'s repeatable part flags
/// (`--text-part`/`--file-part`/`--data-part`/`--media-type`) needs the raw
/// [`ArgMatches`], which `Cli::parse()` alone discards.
pub async fn run_args<I, T>(args: I) -> Result<(), CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    // Must happen before parsing: this is how a local/global .env value
    // becomes visible to clap's own `env = "A2ACLI_..."` resolution (§8.3).
    let dotenv_provenance = load_dotenv_config(&args);
    // `try_get_matches_from` rather than `get_matches_from`: the latter
    // prints clap's prose and exits inside clap, which §11.4 forbids for a
    // malformed flag — that is a CLI-local failure and must still be
    // machine-readable.
    let matches = match Cli::command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error) => return Err(clap_error_to_cli_error(error)),
    };
    let cli = Cli::from_arg_matches(&matches).expect("matches were produced by Cli::command()");
    run(cli, &matches, &dotenv_provenance).await
}

/// clap reports `--help` and `--version` through the same `Err` channel as a
/// parse failure, so they have to be told apart: those are successful
/// requests for output (`A2ACLI_CLI_001`) and keep going to stdout with exit
/// `0`, while everything else is a usage error carrying the Appendix B
/// envelope.
fn clap_error_to_cli_error(error: clap::Error) -> CliError {
    use clap::error::ErrorKind;

    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        // Not failures: the caller asked for this output (A2ACLI_CLI_001).
        // Delegating to clap's own `exit()` keeps them byte-for-byte what
        // `get_matches_from` produced, since that is just
        // `try_get_matches_from(..).unwrap_or_else(|e| e.exit())`.
        error.exit();
    }

    // `DisplayHelpOnMissingArgumentOrSubcommand` — a bare `a2acli`, or a
    // group like `a2acli task` with no subcommand — is *not* in
    // A2ACLI_CLI_001's list of output requests: clap already treats it as a
    // usage failure and exits 2. So it carries the envelope like every
    // other usage error rather than being the one that prints prose, and
    // the hint points at `--help` for the usage text it replaces.

    CliError::Usage(flatten_clap_message(&error.render().to_string()))
}

/// clap renders a multi-line message: the error line, a blank line, then a
/// usage block. Keep the part that names what was wrong, as one line, since
/// §11.4 wants a single JSON object and the usage block is guidance the
/// `hint` already points at.
fn flatten_clap_message(rendered: &str) -> String {
    // Each line is trimmed on both sides before joining: clap indents
    // continuation lines, and keeping that indentation would leave runs of
    // spaces inside the JSON message.
    let first_block: Vec<&str> = rendered
        .lines()
        .map(str::trim)
        .take_while(|line| !line.is_empty())
        .collect();
    let joined = first_block.join(" ");
    let message = if joined.is_empty() {
        rendered.trim()
    } else {
        joined.as_str()
    };
    message
        .strip_prefix("error: ")
        .unwrap_or(message)
        .to_string()
}

pub async fn run(
    cli: Cli,
    matches: &ArgMatches,
    dotenv_provenance: &HashMap<String, ConfigSource>,
) -> Result<(), CliError> {
    if cli.debug {
        // --debug: verbose diagnostics to stderr (§7.2). try_init() rather
        // than init() because a process embedding `run` more than once
        // (e.g. this crate's own in-process tests) would otherwise panic
        // on a second global-subscriber install.
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            // DEBUG for this project's crates, because the raw wire
            // messages §7.2 asks for at Tier 2 are emitted at that level —
            // but not for dependencies, whose connection-pool chatter is
            // also DEBUG and would bury the protocol exchange it is meant
            // to show.
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "warn,a2a_client=debug,a2a_cli=debug",
            ))
            // Colour only when a person is watching: these diagnostics are
            // routinely redirected to a file or piped into a grep, and
            // escape codes there are noise rather than emphasis.
            .with_ansi(std::io::stderr().is_terminal())
            .with_target(false)
            .try_init();
    }

    // §11.6: a bad flag is reported before any network work, so a usage
    // error never depends on a reachable agent.
    validate_a2a_version(cli.a2a_version.as_deref())?;

    match &cli.command {
        Command::Card { command } => run_card_command(&cli, matches, command).await?,
        Command::Config { command } => {
            run_config_command(&cli, matches, dotenv_provenance, command)?
        }
        Command::Completion { shell } => {
            // Deliberately resolves no Agent Card and builds no client: the
            // command has to work with no agent configured at all. The script
            // is meant to be redirected to a file or eval'd, so stdout
            // carries it and nothing else, and -o json does not wrap it
            // (§11.1).
            let mut command = Cli::command();
            let binary = command.get_name().to_string();
            clap_complete::generate(*shell, &mut command, binary, &mut std::io::stdout());
        }
        Command::Send(command) => {
            let send_matches = matches
                .subcommand_matches("send")
                .expect("send subcommand matches present when cli.command is Command::Send");
            let parts = resolve_message_parts(send_matches, command)?;
            let ResolvedClient {
                client,
                tenant,
                capabilities,
            } = resolve_client(&cli, matches).await?;
            let request = build_send_message_request(command, parts, tenant.clone());

            // §13.3 pre-flight: the card is the contract, so a card that
            // does not declare streaming settles the question without a
            // round trip. The caller asked for a result rather than
            // specifically for a stream, so this takes the same fallback
            // #173 gave an UNSUPPORTED_OPERATION from the agent
            // (TASK_POLL_004) instead of failing.
            let streaming = capabilities.declares(Capability::Streaming);
            if command.stream && !streaming {
                eprintln!(
                    "warning: the agent card does not declare streaming; \
                     sending without it and polling for the result instead"
                );
            }

            if command.stream && streaming {
                match client.send_streaming_message(&request).await {
                    Ok(stream) => {
                        consume_stream(client, stream, &cli).await?;
                    }
                    Err(error) if error.code == a2a::error_code::UNSUPPORTED_OPERATION => {
                        // The agent doesn't advertise the streaming
                        // capability — fall back to a one-shot send plus
                        // polling rather than hanging (TASK_POLL_004). Any
                        // *other* error opening the stream is a real
                        // failure and must be reported as such, not masked
                        // by a silent retry.
                        let response = send_and_maybe_wait(
                            client,
                            &request,
                            tenant,
                            cli.async_mode,
                            cli.poll_interval,
                            cli.timeout,
                        )
                        .await?;
                        response.warn_outcome();
                        print_output(&response, &cli)?;
                    }
                    Err(error) => {
                        let _ = client.destroy().await;
                        return Err(error.into());
                    }
                }
            } else {
                let response = send_and_maybe_wait(
                    client,
                    &request,
                    tenant,
                    cli.async_mode,
                    cli.poll_interval,
                    cli.timeout,
                )
                .await?;
                response.warn_outcome();
                print_output(&response, &cli)?;
            }
        }
        Command::Task { command } => run_task_command(&cli, matches, command).await?,
    }

    Ok(())
}

/// Send a message and, unless `--async` was given, block until the resulting
/// task reaches a terminal or interrupted state (§6.5/§9.3 default-wait
/// behavior). A `Message`-only response has no task to wait on and is
/// returned as soon as it arrives. Always destroys `client` before returning.
async fn send_and_maybe_wait<T: a2a_client::Transport>(
    client: A2AClient<T>,
    request: &SendMessageRequest,
    tenant: Option<String>,
    async_mode: bool,
    poll_interval: Duration,
    timeout: Duration,
) -> Result<SendMessageResponse, CliError> {
    let result = client.send_message(request).await;
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            let _ = client.destroy().await;
            return Err(error.into());
        }
    };

    match response {
        SendMessageResponse::Task(task) => {
            let task =
                settle_task(client, task, !async_mode, tenant, poll_interval, timeout).await?;
            Ok(SendMessageResponse::Task(task))
        }
        SendMessageResponse::Message(message) => {
            client.destroy().await?;
            Ok(SendMessageResponse::Message(message))
        }
    }
}

/// If `wait` is true and `task` has not already settled, poll until it
/// reaches a terminal or interrupted state (or `timeout` expires). Destroys
/// `client` before returning either way.
async fn settle_task<T: a2a_client::Transport>(
    client: A2AClient<T>,
    task: Task,
    wait: bool,
    tenant: Option<String>,
    poll_interval: Duration,
    timeout: Duration,
) -> Result<Task, CliError> {
    let outcome = if wait && !is_settled(&task.status.state) {
        wait_for_task_settled(&client, &task.id, tenant, poll_interval, timeout).await
    } else {
        Ok(task)
    };

    match outcome {
        Ok(task) => {
            client.destroy().await?;
            Ok(task)
        }
        Err(error) => {
            let _ = client.destroy().await;
            Err(error)
        }
    }
}

/// Poll `task get` until the task reaches a terminal or interrupted state
/// (§9.1/§9.3), sleeping `poll_interval` between attempts and giving up with
/// [`CliError::Timeout`] once `timeout` has elapsed. `TASK_STATE_UNSPECIFIED`
/// is treated as neither terminal nor interrupted, so it keeps polling.
async fn wait_for_task_settled<T: a2a_client::Transport>(
    client: &A2AClient<T>,
    task_id: &str,
    tenant: Option<String>,
    poll_interval: Duration,
    timeout: Duration,
) -> Result<Task, CliError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let task = client
            .get_task(&GetTaskRequest {
                id: task_id.to_string(),
                history_length: None,
                tenant: tenant.clone(),
            })
            .await?;

        if is_settled(&task.status.state) {
            return Ok(task);
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(CliError::Timeout {
                task_id: task_id.to_string(),
                timeout,
            });
        }

        tokio::time::sleep(poll_interval.min(deadline.saturating_duration_since(now))).await;
    }
}

/// §6.6/§11.6: a turn the CLI conducted and reported exits `0` even when the
/// agent did not succeed, so the exit status alone cannot tell a caller
/// that. Name a non-success or paused outcome on stderr instead — in every
/// output mode, like the error envelope (§11.4), and without touching
/// stdout, which stays the payload (§11.1).
///
/// The four states named are exactly the ones `A2ACLI_EXIT_002` enumerates.
/// `CANCELED` is deliberately not among them: after `task cancel` it is the
/// outcome the caller asked for, so warning about it would be noise.
fn task_outcome_note(state: &TaskState) -> Option<&'static str> {
    match state {
        TaskState::Failed => Some("the agent reported it FAILED"),
        TaskState::Rejected => Some("the agent REJECTED it"),
        TaskState::InputRequired => {
            Some("it is paused at INPUT_REQUIRED and needs a reply to continue")
        }
        TaskState::AuthRequired => {
            Some("it is paused at AUTH_REQUIRED and needs credentials to continue")
        }
        _ => None,
    }
}

fn warn_task_outcome(task_id: &str, state: &TaskState) {
    if let Some(note) = task_outcome_note(state) {
        eprintln!("warning: task {task_id}: {note}");
    }
}

/// Something the CLI reports that may carry a task outcome. A trait rather
/// than a call at each print site so a new reporting path cannot quietly
/// skip the warning — it has to satisfy the bound.
trait TaskOutcome {
    fn warn_outcome(&self);
}

impl TaskOutcome for Task {
    fn warn_outcome(&self) {
        warn_task_outcome(&self.id, &self.status.state);
    }
}

impl TaskOutcome for SendMessageResponse {
    fn warn_outcome(&self) {
        match self {
            SendMessageResponse::Task(task) => task.warn_outcome(),
            // A `Message`-only reply created no task, so there is no task
            // outcome to report (SEND_003).
            SendMessageResponse::Message(_) => {}
        }
    }
}

impl TaskOutcome for StreamResponse {
    fn warn_outcome(&self) {
        match self {
            StreamResponse::Task(task) => task.warn_outcome(),
            StreamResponse::StatusUpdate(event) => {
                warn_task_outcome(&event.task_id, &event.status.state);
            }
            StreamResponse::Message(_) | StreamResponse::ArtifactUpdate(_) => {}
        }
    }
}

/// A task in a terminal (`COMPLETED`/`FAILED`/`CANCELED`/`REJECTED`) or
/// interrupted (`INPUT_REQUIRED`/`AUTH_REQUIRED`) state needs no more polling.
fn is_settled(state: &TaskState) -> bool {
    state.is_terminal() || matches!(state, TaskState::InputRequired | TaskState::AuthRequired)
}

async fn run_card_command(
    cli: &Cli,
    matches: &ArgMatches,
    command: &CardCommand,
) -> Result<(), CliError> {
    match command {
        CardCommand::Get(command) => {
            if command.extended {
                let ResolvedClient {
                    client,
                    tenant,
                    capabilities,
                } = resolve_client(cli, matches).await?;
                // No equivalent path exists for an extended card, so this
                // fails with the reason instead of asking anyway.
                capabilities.require(Capability::ExtendedCard)?;
                let result = client
                    .get_extended_agent_card(&GetExtendedAgentCardRequest { tenant })
                    .await;
                let card = finish_client_call(client, result).await?;
                if command.validate {
                    // Best effort: get_extended_agent_card comes back
                    // through a2a-client's typed protojson pipeline, which
                    // has already discarded anything the schema would flag
                    // as an unrecognised property, the same way AgentCard's
                    // own Deserialize would. Re-serializing the typed value
                    // still catches a wrong type or a bad enum value; it
                    // cannot catch that one shape of violation on this path.
                    validate_card_or_fail(&serde_json::to_value(&card)?)?;
                }
                print_output(&card, cli)?;
            } else {
                let (card, raw) = resolve_agent_card_with_raw(cli, matches).await?;
                if command.validate {
                    validate_card_or_fail(&raw)?;
                }
                print_output(&card, cli)?;
            }
        }
    }

    Ok(())
}

/// `card get --validate` (§10.1, `A2ACLI_CARD_GET_002`). Every violation is
/// collected before failing, rather than stopping at the first — a card with
/// three problems should take one run to diagnose.
fn validate_card_or_fail(card: &Value) -> Result<(), CliError> {
    let violations = card_schema::validate_agent_card(card)
        .map_err(|error| CliError::CardInvalid(format!("schema could not be loaded: {error}")))?;
    if violations.is_empty() {
        Ok(())
    } else {
        Err(CliError::CardSchemaInvalid { violations })
    }
}

async fn run_task_command(
    cli: &Cli,
    matches: &ArgMatches,
    command: &TaskCommand,
) -> Result<(), CliError> {
    match command {
        TaskCommand::Get(command) => {
            let ResolvedClient { client, tenant, .. } = resolve_client(cli, matches).await?;
            let result = client
                .get_task(&GetTaskRequest {
                    id: command.id.clone(),
                    history_length: command.history_length,
                    tenant: tenant.clone(),
                })
                .await;
            let task = match result {
                Ok(task) => task,
                Err(error) => {
                    let _ = client.destroy().await;
                    return Err(error.into());
                }
            };
            let task = settle_task(
                client,
                task,
                cli.wait,
                tenant,
                cli.poll_interval,
                cli.timeout,
            )
            .await?;
            task.warn_outcome();
            print_output(&task, cli)?;
        }
        TaskCommand::List(command) => {
            let ResolvedClient { client, tenant, .. } = resolve_client(cli, matches).await?;
            let result = client
                .list_tasks(&ListTasksRequest {
                    context_id: command.context_id.clone(),
                    status: command.status.map(TaskState::from),
                    page_size: command.page_size,
                    page_token: command.page_token.clone(),
                    history_length: command.history_length,
                    status_timestamp_after: None,
                    include_artifacts: command.include_artifacts.then_some(true),
                    tenant,
                })
                .await;
            let response = finish_client_call(client, result).await?;
            print_output(&response, cli)?;
        }
        TaskCommand::Cancel(command) => {
            let ResolvedClient { client, tenant, .. } = resolve_client(cli, matches).await?;
            let result = client
                .cancel_task(&CancelTaskRequest {
                    id: command.id.clone(),
                    metadata: None,
                    tenant,
                })
                .await;
            let task = finish_client_call(client, result).await?;
            task.warn_outcome();
            print_output(&task, cli)?;
        }
        TaskCommand::Subscribe(command) => {
            let ResolvedClient {
                client,
                tenant,
                capabilities,
            } = resolve_client(cli, matches).await?;
            // Unlike `send --stream`, subscribing *is* the request: there is
            // no non-streaming way to satisfy it, so an undeclared
            // capability is a failure rather than a fallback.
            capabilities.require(Capability::Streaming)?;
            let stream = client
                .subscribe_to_task(&SubscribeToTaskRequest {
                    id: command.id.clone(),
                    tenant: tenant.clone(),
                })
                .await?;
            subscribe_with_resumption(client, stream, command.id.clone(), tenant, cli).await?;
        }
        TaskCommand::PushConfig { command } => {
            run_push_config_command(cli, matches, command).await?;
        }
    }

    Ok(())
}

/// Reconstruct `send`'s message parts in the order the caller typed them,
/// binding each `--media-type` to the part flag immediately preceding it.
///
/// `clap`'s derived `MessageCommand` struct only exposes each repeatable flag
/// (`--text-part`, `--file-part`, `--data-part`, `--media-type`) as its own
/// `Vec<String>`, which loses the relative order *across* flags. Recovering
/// that order needs the raw [`ArgMatches`] for the `send` subcommand:
/// `indices_of` reports, for a given arg id, the token index of each value in
/// the original command line, which sorts back into the order the caller
/// wrote them (§10.2: "the part flags are repeatable and order-preserving").
fn resolve_message_parts(
    matches: &ArgMatches,
    command: &MessageCommand,
) -> Result<Vec<Part>, CliError> {
    enum Kind {
        Text,
        File,
        Data,
    }
    enum Token<'a> {
        Part(Kind, &'a str),
        MediaType(&'a str),
    }

    let mut tokens: Vec<(usize, Token<'_>)> = Vec::new();
    if let Some(indices) = matches.indices_of("text_parts") {
        tokens.extend(
            indices
                .zip(command.text_parts.iter())
                .map(|(index, value)| (index, Token::Part(Kind::Text, value.as_str()))),
        );
    }
    if let Some(indices) = matches.indices_of("file_parts") {
        tokens.extend(
            indices
                .zip(command.file_parts.iter())
                .map(|(index, value)| (index, Token::Part(Kind::File, value.as_str()))),
        );
    }
    if let Some(indices) = matches.indices_of("data_parts") {
        tokens.extend(
            indices
                .zip(command.data_parts.iter())
                .map(|(index, value)| (index, Token::Part(Kind::Data, value.as_str()))),
        );
    }

    if !command.media_types.is_empty() && tokens.is_empty() {
        // A lone --media-type with no other part flag at all must not be
        // silently dropped by the "no part flags given" branch below.
        return Err(CliError::InvalidInput(
            "--media-type must immediately follow a --text-part/--file-part/--data-part"
                .to_string(),
        ));
    }

    if tokens.is_empty() {
        // No part flags given: the positional text (if any) is shorthand for
        // a single text part. A message with no content at all is a usage
        // error, not a silently empty parts array.
        return match command.text.as_deref() {
            Some(text) => Ok(vec![Part::text(text)]),
            None => Err(CliError::InvalidInput(
                "message must have at least one part: pass text, or use \
                 --text-part/--file-part/--data-part"
                    .to_string(),
            )),
        };
    }

    if command.text.is_some() {
        return Err(CliError::InvalidInput(
            "the positional message text cannot be combined with \
             --text-part/--file-part/--data-part; pass it as --text-part instead"
                .to_string(),
        ));
    }

    if let Some(indices) = matches.indices_of("media_types") {
        tokens.extend(
            indices
                .zip(command.media_types.iter())
                .map(|(index, value)| (index, Token::MediaType(value.as_str()))),
        );
    }
    tokens.sort_by_key(|(index, _)| *index);

    let mut parts: Vec<Part> = Vec::new();
    for (_, token) in tokens {
        match token {
            Token::Part(kind, value) => {
                let part = match kind {
                    Kind::Text => Part::text(value),
                    Kind::File => build_file_part(value)?,
                    Kind::Data => build_data_part(value)?,
                };
                parts.push(part);
            }
            Token::MediaType(media_type) => match parts.last_mut() {
                Some(part) => part.media_type = Some(media_type.to_string()),
                None => {
                    return Err(CliError::InvalidInput(
                        "--media-type must immediately follow a --text-part/--file-part/--data-part"
                            .to_string(),
                    ));
                }
            },
        }
    }

    Ok(parts)
}

/// A local filesystem path becomes an inline, base64-encoded file part; a URL
/// is carried by reference and never fetched by the CLI (§10.2).
fn build_file_part(value: &str) -> Result<Part, CliError> {
    if value.contains("://") {
        return Ok(Part::url(value));
    }

    let bytes = std::fs::read(value).map_err(|source| CliError::ReadFile {
        path: value.to_string(),
        source,
    })?;
    let mut part = Part::raw(bytes);
    part.filename = std::path::Path::new(value)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    Ok(part)
}

/// `--data-part <path|->` reads JSON from a file path or, for `-`, from
/// stdin; anything else is parsed as an inline JSON string (§10.2).
/// Whether a `read_to_string` failure means "this string doesn't name a
/// readable file here" — so `--data-part` should go on to try parsing it as
/// inline JSON — rather than "a real file failed to read", which must be
/// reported.
///
/// This can't just test for `NotFound`: Windows rejects a path containing
/// characters it doesn't allow (`{`, `"`, `:` — precisely what inline JSON
/// looks like) with `InvalidFilename` *before* ever looking for the file, so
/// matching only `NotFound` would turn every inline-JSON `--data-part` into
/// a read error there while working fine on Unix, where those are all legal
/// filename characters.
fn means_not_a_readable_path(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::InvalidFilename
            | std::io::ErrorKind::InvalidInput
    )
}

fn build_data_part(value: &str) -> Result<Part, CliError> {
    if value == "-" {
        use std::io::Read;
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|source| CliError::ReadFile {
                path: "<stdin>".to_string(),
                source,
            })?;
        return parse_data_part_json(&buffer, "<stdin>");
    }

    match std::fs::read_to_string(value) {
        Ok(text) => return parse_data_part_json(&text, value),
        Err(source) if !means_not_a_readable_path(source.kind()) => {
            // A real file that couldn't be read (permission denied, a
            // directory, invalid UTF-8, ...) — that's a failure to
            // surface, not a signal to fall back to inline-JSON parsing.
            return Err(CliError::ReadFile {
                path: value.to_string(),
                source,
            });
        }
        Err(_) => {} // doesn't name a readable file — try `value` as JSON below
    }

    serde_json::from_str(value).map(Part::data).map_err(|_| {
        CliError::InvalidInput(format!(
            "--data-part must be a file path, \"-\" for stdin, or inline JSON: {value}"
        ))
    })
}

/// Parse `text` (the content of a `--data-part` file or stdin) as JSON,
/// mapping a parse failure to a usage error naming `source` rather than the
/// generic internal `CliError::Json` — the caller supplied this content, so
/// a malformed payload is their input to fix, not the tool's own failure.
fn parse_data_part_json(text: &str, source: &str) -> Result<Part, CliError> {
    serde_json::from_str(text).map(Part::data).map_err(|error| {
        CliError::InvalidInput(format!("--data-part: invalid JSON in {source}: {error}"))
    })
}

/// Parse a duration like "2s", "500ms", "1m", or a bare number of seconds.
fn parse_duration(input: &str) -> Result<Duration, String> {
    let trimmed = input.trim();
    let (magnitude, unit) = if let Some(value) = trimmed.strip_suffix("ms") {
        (value, "ms")
    } else if let Some(value) = trimmed.strip_suffix('s') {
        (value, "s")
    } else if let Some(value) = trimmed.strip_suffix('m') {
        (value, "m")
    } else {
        (trimmed, "s")
    };

    let magnitude: f64 = magnitude
        .trim()
        .parse()
        .map_err(|_| format!("invalid duration: {input}"))?;
    if magnitude < 0.0 || !magnitude.is_finite() {
        return Err(format!("duration must be a non-negative number: {input}"));
    }

    let seconds = match unit {
        "ms" => magnitude / 1000.0,
        "m" => magnitude * 60.0,
        _ => magnitude,
    };
    Ok(Duration::from_secs_f64(seconds))
}

fn build_send_message_request(
    command: &MessageCommand,
    parts: Vec<Part>,
    tenant: Option<String>,
) -> SendMessageRequest {
    let mut message = Message::new(Role::User, parts);
    message.context_id = command.context_id.clone();
    message.task_id = command.task_id.clone();

    let configuration = if command.history_length.is_some()
        || !command.accepted_output_modes.is_empty()
        || command.return_immediately
    {
        Some(SendMessageConfiguration {
            accepted_output_modes: (!command.accepted_output_modes.is_empty())
                .then_some(command.accepted_output_modes.clone()),
            task_push_notification_config: None,
            history_length: command.history_length,
            return_immediately: command.return_immediately.then_some(true),
        })
    } else {
        None
    };

    SendMessageRequest {
        message,
        configuration,
        metadata: None,
        tenant,
    }
}

fn build_push_notification_config(
    command: &CreatePushConfigCommand,
) -> Result<TaskPushNotificationConfig, CliError> {
    if command.auth_credentials.is_some() && command.auth_scheme.is_none() {
        return Err(CliError::InvalidInput(
            "--auth-credentials requires --auth-scheme".to_string(),
        ));
    }

    let authentication = command
        .auth_scheme
        .clone()
        .map(|scheme| AuthenticationInfo {
            scheme,
            credentials: command.auth_credentials.clone(),
        });

    Ok(TaskPushNotificationConfig {
        task_id: String::new(),
        url: command.url.clone(),
        id: command.config_id.clone(),
        token: command.token.clone(),
        authentication,
        tenant: None,
    })
}

/// Every push-notification subcommand is gated on the card declaring
/// `pushNotifications` (§13.3). None of them has a non-push equivalent, so
/// an undeclared capability fails with the reason rather than spending a
/// round trip to be told the same thing less clearly.
async fn run_push_config_command(
    cli: &Cli,
    matches: &ArgMatches,
    command: &PushConfigCommand,
) -> Result<(), CliError> {
    match command {
        PushConfigCommand::Create(command) => {
            let ResolvedClient {
                client,
                tenant,
                capabilities,
            } = resolve_client(cli, matches).await?;
            capabilities.require(Capability::PushNotifications)?;
            let mut config = build_push_notification_config(command)?;
            config.task_id = command.task_id.clone();
            config.tenant = tenant;
            let result = client.create_push_config(&config).await;
            let response = finish_client_call(client, result).await?;
            print_output(&response, cli)?;
        }
        PushConfigCommand::Get(command) => {
            let ResolvedClient {
                client,
                tenant,
                capabilities,
            } = resolve_client(cli, matches).await?;
            capabilities.require(Capability::PushNotifications)?;
            let result = client
                .get_push_config(&GetTaskPushNotificationConfigRequest {
                    task_id: command.task_id.clone(),
                    id: command.id.clone(),
                    tenant,
                })
                .await;
            let response = finish_client_call(client, result).await?;
            print_output(&response, cli)?;
        }
        PushConfigCommand::List(command) => {
            let ResolvedClient {
                client,
                tenant,
                capabilities,
            } = resolve_client(cli, matches).await?;
            capabilities.require(Capability::PushNotifications)?;
            let result = client
                .list_push_configs(&ListTaskPushNotificationConfigsRequest {
                    task_id: command.task_id.clone(),
                    page_size: command.page_size,
                    page_token: command.page_token.clone(),
                    tenant,
                })
                .await;
            let response = finish_client_call(client, result).await?;
            print_output(&response, cli)?;
        }
        PushConfigCommand::Delete(command) => {
            let ResolvedClient {
                client,
                tenant,
                capabilities,
            } = resolve_client(cli, matches).await?;
            capabilities.require(Capability::PushNotifications)?;
            let result = client
                .delete_push_config(&DeleteTaskPushNotificationConfigRequest {
                    task_id: command.task_id.clone(),
                    id: command.id.clone(),
                    tenant,
                })
                .await;
            finish_client_call(client, result).await?;
            print_output(
                &PushConfigDeleted {
                    deleted: true,
                    task_id: command.task_id.clone(),
                    id: command.id.clone(),
                },
                cli,
            )?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------
// Configuration (§8.3): .env discovery/precedence and `config show`.
// ---------------------------------------------------------------------

/// Where an effective setting's value came from (§8.3's precedence order,
/// highest first): an explicit flag, a real environment variable, a local
/// `.env` file, a global `.env` file, or the tool's built-in default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Flag,
    EnvVar,
    LocalFile,
    GlobalFile,
    Default,
}

impl ConfigSource {
    fn label(self) -> &'static str {
        match self {
            ConfigSource::Flag => "flag",
            ConfigSource::EnvVar => "environment variable",
            ConfigSource::LocalFile => "local .env file",
            ConfigSource::GlobalFile => "global .env file",
            ConfigSource::Default => "built-in default",
        }
    }
}

/// Load `.env` configuration and inject any `A2ACLI_*` key that isn't
/// already a real environment variable, so clap's own `env = "..."`
/// resolution on [`Cli`]'s fields sees it — this MUST run before
/// `Cli::command().get_matches_from(args)`. Returns which keys were
/// injected from which file, for `config show` to report accurately.
///
/// Precedence (§6.5, §8.3): a real environment variable always wins over
/// either file; a local `.env` (or the file named by `--config`) wins over
/// the global `~/.config/a2a-cli/.env`.
fn load_dotenv_config(args: &[OsString]) -> HashMap<String, ConfigSource> {
    // Snapshot which A2ACLI_* keys are *real* environment variables before
    // any file-based injection, so a lower-precedence file's value is never
    // mistaken for "already set" once a higher-precedence file has run.
    let real_env_keys: HashSet<String> = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| key.starts_with("A2ACLI_"))
        .collect();

    let mut provenance = HashMap::new();

    if let Some(global_path) = global_config_path() {
        apply_dotenv_file(
            &global_path,
            ConfigSource::GlobalFile,
            &real_env_keys,
            &mut provenance,
        );
    }

    let local_path = find_flag_value(args, "--config")
        .map(PathBuf::from)
        .or_else(find_local_dotenv);
    if let Some(local_path) = local_path {
        apply_dotenv_file(
            &local_path,
            ConfigSource::LocalFile,
            &real_env_keys,
            &mut provenance,
        );
    }

    provenance
}

/// Look for `--flag value` or `--flag=value` in a raw, unparsed argv —
/// needed to find `--config` before `Cli` itself has been parsed.
fn find_flag_value(args: &[OsString], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    for (index, arg) in args.iter().enumerate() {
        let Some(arg) = arg.to_str() else { continue };
        if arg == flag {
            return args.get(index + 1)?.to_str().map(str::to_string);
        }
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}

/// `~/.config/a2a-cli/.env`, honoring `$XDG_CONFIG_HOME`.
fn global_config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(xdg).join("a2a-cli").join(".env"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("a2a-cli")
            .join(".env"),
    )
}

/// A local `.env`, discovered the way `git` discovers its configuration:
/// walking up from the working directory (§8.3).
fn find_local_dotenv() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(".env");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn apply_dotenv_file(
    path: &Path,
    source: ConfigSource,
    real_env_keys: &HashSet<String>,
    provenance: &mut HashMap<String, ConfigSource>,
) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    warn_if_world_readable(path);

    for (key, value) in parse_dotenv(&contents) {
        if !key.starts_with("A2ACLI_") || real_env_keys.contains(&key) {
            continue;
        }
        // SAFETY: this runs synchronously, at the very start of `run_args`,
        // before any async work or additional threads have been spawned by
        // this process — nothing else can be concurrently reading or
        // writing the environment at this point.
        unsafe {
            std::env::set_var(&key, &value);
        }
        provenance.insert(key, source);
    }
}

/// §8.3: "MUST NOT store secrets in world-readable files". `a2acli` never
/// writes a `.env` file itself, but it can at least warn when one it reads
/// is readable by users other than its owner.
#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        eprintln!(
            "warning: {} is readable by other users (mode {:o}); it may contain credentials — \
             consider `chmod 600 {}`",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}

/// Parse `.env` (dotenv) syntax: `KEY=value`, one per line. Blank lines and
/// `#` comments are ignored, a leading `export ` is tolerated, and one
/// layer of surrounding single or double quotes is stripped (§8.3).
fn parse_dotenv(contents: &str) -> Vec<(String, String)> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let key = key.trim().to_string();
            let value = value.trim();
            let value = ["\"", "'"]
                .iter()
                .find_map(|quote| {
                    value
                        .strip_prefix(quote)
                        .and_then(|rest| rest.strip_suffix(quote))
                })
                .unwrap_or(value);
            Some((key, value.to_string()))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
struct ConfigSetting {
    name: String,
    value: String,
    source: String,
}

#[derive(Debug, Clone, Serialize)]
struct ConfigSettings {
    settings: Vec<ConfigSetting>,
}

impl TextRender for ConfigSettings {
    fn render_text(&self) -> String {
        let mut out = TextOutput::default();
        for setting in &self.settings {
            out.field(
                &setting.name,
                format!("{} (source: {})", setting.value, setting.source),
            );
        }
        out.finish()
    }
}

fn redact_secret(value: &Option<String>) -> String {
    match value {
        Some(_) => "(set, redacted)".to_string(),
        None => "(not set)".to_string(),
    }
}

fn run_config_command(
    cli: &Cli,
    matches: &ArgMatches,
    dotenv_provenance: &HashMap<String, ConfigSource>,
    command: &ConfigCommand,
) -> Result<(), CliError> {
    match command {
        ConfigCommand::Show => {
            let settings = effective_config_settings(cli, matches, dotenv_provenance);
            print_output(&settings, cli)?;
        }
    }
    Ok(())
}

/// Enumerate every §6.5/§8.3 configurable setting with its current value
/// and the source it resolved from, for `config show`.
fn effective_config_settings(
    cli: &Cli,
    matches: &ArgMatches,
    dotenv_provenance: &HashMap<String, ConfigSource>,
) -> ConfigSettings {
    let mut settings = Vec::new();
    let mut add = |id: &str, name: &str, value: String, env_key: &str| {
        let source = match matches.value_source(id) {
            Some(ValueSource::CommandLine) => ConfigSource::Flag,
            Some(ValueSource::EnvVariable) => dotenv_provenance
                .get(env_key)
                .copied()
                .unwrap_or(ConfigSource::EnvVar),
            _ => ConfigSource::Default,
        };
        settings.push(ConfigSetting {
            name: name.to_string(),
            value,
            source: source.label().to_string(),
        });
    };

    add(
        "agent_card",
        "agent-card",
        cli.agent_card
            .clone()
            .unwrap_or_else(|| "(not set)".to_string()),
        "A2ACLI_AGENT_CARD",
    );
    add(
        "endpoint",
        "endpoint",
        cli.endpoint
            .clone()
            .unwrap_or_else(|| "(not set)".to_string()),
        "A2ACLI_ENDPOINT",
    );
    add(
        "base_url",
        "base-url",
        cli.base_url.clone(),
        "A2ACLI_BASE_URL",
    );
    add(
        "transport",
        "transport",
        if cli.transport.is_empty() {
            "(agent card's own order)".to_string()
        } else {
            cli.transport
                .iter()
                .map(|binding| format!("{binding:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(",")
        },
        "A2ACLI_TRANSPORT",
    );
    add(
        "a2a_version",
        "a2a-version",
        cli.a2a_version.clone().unwrap_or_else(|| {
            format!("(negotiated from the agent card; a2acli supports {VERSION})")
        }),
        "A2ACLI_A2A_VERSION",
    );
    add(
        "bearer",
        "bearer",
        redact_secret(&cli.bearer),
        "A2ACLI_BEARER",
    );
    add(
        "api_key",
        "api-key",
        redact_secret(&cli.api_key),
        "A2ACLI_API_KEY",
    );
    add(
        "insecure",
        "insecure",
        cli.insecure.to_string(),
        "A2ACLI_INSECURE",
    );
    add("debug", "debug", cli.debug.to_string(), "A2ACLI_DEBUG");
    add(
        "tenant",
        "tenant",
        cli.tenant
            .clone()
            .unwrap_or_else(|| "(not set)".to_string()),
        "A2ACLI_TENANT",
    );
    add(
        "output",
        "output",
        format!("{:?}", cli.output).to_lowercase(),
        "A2ACLI_OUTPUT",
    );
    add(
        "compact",
        "compact",
        cli.compact.to_string(),
        "A2ACLI_COMPACT",
    );
    add(
        "async_mode",
        "async",
        cli.async_mode.to_string(),
        "A2ACLI_ASYNC",
    );
    add("wait", "wait", cli.wait.to_string(), "A2ACLI_WAIT");
    add(
        "poll_interval",
        "poll-interval",
        format!("{:?}", cli.poll_interval),
        "A2ACLI_POLL_INTERVAL",
    );
    add(
        "timeout",
        "timeout",
        format!("{:?}", cli.timeout),
        "A2ACLI_TIMEOUT",
    );

    // Where the card is actually fetched from, given the above: the one
    // line that answers "which agent am I talking to?" without the reader
    // having to apply the precedence rules themselves.
    settings.push(ConfigSetting {
        name: "resolved-card".to_string(),
        value: match &cli.endpoint {
            Some(endpoint) => format!("(none; --endpoint {endpoint})"),
            None => {
                match normalize_card_reference(cli.agent_card.as_deref().unwrap_or(&cli.base_url)) {
                    CardReference::File(path) => format!("file://{}", path.display()),
                    CardReference::Url(url) => url,
                }
            }
        },
        source: "derived".to_string(),
    });

    ConfigSettings { settings }
}

/// A negotiated client, plus the tenant to attach to every subsequent
/// request on it: the caller's explicit `--tenant` when given, otherwise
/// (per A2A §8.3.2) the routing tenant the *selected* Agent Card interface
/// itself declares, if any (TX_003).
struct ResolvedClient {
    client: A2AClient<Box<dyn a2a_client::Transport>>,
    tenant: Option<String>,
    capabilities: DeclaredCapabilities,
}

/// A capability the specification gates an operation on (§13.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capability {
    Streaming,
    PushNotifications,
    ExtendedCard,
}

impl Capability {
    /// Read the way the card renderer reads it: an absent field is not a
    /// declaration, so `None` and `Some(false)` are the same answer.
    fn declared_in(self, capabilities: &AgentCapabilities) -> bool {
        match self {
            Capability::Streaming => capabilities.streaming.unwrap_or(false),
            Capability::PushNotifications => capabilities.push_notifications.unwrap_or(false),
            Capability::ExtendedCard => capabilities.extended_agent_card.unwrap_or(false),
        }
    }

    /// The card field's name, as it is spelled in the card.
    fn field(self) -> &'static str {
        match self {
            Capability::Streaming => "streaming",
            Capability::PushNotifications => "pushNotifications",
            Capability::ExtendedCard => "extendedAgentCard",
        }
    }

    /// The protocol error the agent itself would have returned, so that
    /// answering locally changes only the round trip and the message, never
    /// the code a caller branches on.
    fn undeclared_error(self) -> A2AError {
        let message = format!("the agent card does not declare {}", self.field());
        match self {
            // A2A gives this case its own code, so use it rather than the
            // generic one.
            Capability::PushNotifications => {
                A2AError::new(a2a::error_code::PUSH_NOTIFICATION_NOT_SUPPORTED, message)
            }
            // Deliberately not EXTENDED_CARD_NOT_CONFIGURED: that means the
            // agent offers extended cards and this deployment has none. A
            // card that does not declare the capability is saying the agent
            // does not offer them at all, which is UNSUPPORTED_OPERATION.
            Capability::Streaming | Capability::ExtendedCard => {
                A2AError::unsupported_operation(message)
            }
        }
    }
}

/// What the resolved card is able to say about capabilities.
#[derive(Debug, Clone, PartialEq)]
enum DeclaredCapabilities {
    /// Read from an Agent Card, so its declarations are the contract.
    Card(AgentCapabilities),
    /// `--endpoint` was used, so no card was read. The synthesized card
    /// declares nothing precisely because nothing was read, and gating on
    /// that would refuse operations the agent may well support — so the
    /// pre-flight stands down and the agent answers for itself.
    NoCard,
}

impl DeclaredCapabilities {
    fn declares(&self, capability: Capability) -> bool {
        match self {
            DeclaredCapabilities::Card(capabilities) => capability.declared_in(capabilities),
            DeclaredCapabilities::NoCard => true,
        }
    }

    /// §13.3: refuse a capability-gated call the card says will not work,
    /// rather than spending a round trip to be told so.
    fn require(&self, capability: Capability) -> Result<(), CliError> {
        if self.declares(capability) {
            Ok(())
        } else {
            Err(CliError::A2A(capability.undeclared_error()))
        }
    }
}

/// §13.2: the A2A protocol version, as a `(major, minor)` pair. A2A
/// versions are `major.minor`; a trailing patch component is tolerated and
/// ignored so a card declaring `1.0.2` still negotiates.
fn parse_protocol_version(value: &str) -> Option<(u32, u32)> {
    let mut parts = value.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = match parts.next() {
        Some(minor) => minor.parse().ok()?,
        None => 0,
    };
    Some((major, minor))
}

/// The version this build of a2acli speaks.
fn supported_protocol_version() -> (u32, u32) {
    parse_protocol_version(VERSION).unwrap_or((1, 0))
}

/// What the negotiation settled on, and why — kept together so the caller
/// can report a downgrade rather than applying one silently (§13.2).
struct NegotiatedVersion {
    version: String,
    /// Set when the effective version is not simply this build's own, so
    /// the reason can be surfaced on stderr.
    note: Option<String>,
}

/// §13.2: an explicit `--a2a-version` is signaled as given — the caller
/// asked for it. Absent one, negotiate down to the highest version both
/// a2acli and the agent's selected interface declare, bounded to 1.x and
/// never below 1.0, since A2A reads a pre-1.0 value as 0.3 and a downgrade
/// into 0.x semantics is exactly what §13.2 forbids.
fn negotiate_a2a_version(
    explicit: Option<&str>,
    declared: &str,
    supported: (u32, u32),
) -> NegotiatedVersion {
    // An explicit version is signaled exactly as the caller wrote it. It has
    // already been rejected unless 1.x by `validate_a2a_version`, which runs
    // before any network work so a bad flag needs no reachable agent.
    let supported_label = format!("{}.{}", supported.0, supported.1);

    if let Some(requested) = explicit {
        let note = (parse_protocol_version(requested) != Some(supported)).then(|| {
            format!("signaling A2A-Version {requested} (a2acli supports {supported_label})")
        });
        return NegotiatedVersion {
            version: requested.to_string(),
            note,
        };
    }

    let Some(declared_version) = parse_protocol_version(declared) else {
        return NegotiatedVersion {
            version: supported_label.clone(),
            note: Some(format!(
                "agent card declares an unparseable protocol version ({declared}); \
                 signaling {supported_label}"
            )),
        };
    };

    // Below 1.0, or a different major: there is nothing to negotiate within
    // 1.x, so hold the floor rather than following the card down.
    if declared_version.0 != 1 {
        return NegotiatedVersion {
            version: supported_label.clone(),
            note: Some(format!(
                "agent card declares protocol version {declared}, which is outside 1.x; \
                 signaling {supported_label} rather than negotiating below 1.0"
            )),
        };
    }

    if declared_version < supported {
        let version = format!("{}.{}", declared_version.0, declared_version.1);
        return NegotiatedVersion {
            note: Some(format!(
                "negotiated A2A-Version down to {version} (agent card declares {version}, \
                 a2acli supports {supported_label})"
            )),
            version,
        };
    }

    NegotiatedVersion {
        version: supported_label,
        note: None,
    }
}

/// §13.2 / §11.6: reject a `--a2a-version` a2acli will not signal, before
/// any network work — a bad flag is a usage error and must not require a
/// reachable agent to report. A2A reads an empty or pre-1.0 value as 0.3, so
/// anything outside 1.x is refused rather than quietly downgraded.
fn validate_a2a_version(value: Option<&str>) -> Result<(), CliError> {
    let Some(requested) = value else {
        return Ok(());
    };
    match parse_protocol_version(requested) {
        Some((1, _)) => Ok(()),
        Some(_) => Err(CliError::InvalidInput(format!(
            "--a2a-version must be 1.x, got {requested}: A2A reads an empty or pre-1.0 \
             version as 0.3, and a2acli never signals below 1.0"
        ))),
        None => Err(CliError::InvalidInput(format!(
            "--a2a-version must be a version like 1.0, got {requested}"
        ))),
    }
}

/// Sets `A2A-Version` to exactly the negotiated version, replacing the
/// client library's default rather than appending to it: §13.2 requires one
/// explicit version per request, and two values would leave which one
/// applies undefined.
struct VersionInterceptor {
    version: String,
}

#[async_trait::async_trait]
impl a2a_client::middleware::CallInterceptor for VersionInterceptor {
    async fn before(
        &self,
        _method: &str,
        params: &mut a2a_client::ServiceParams,
    ) -> Result<(), A2AError> {
        params.insert(SVC_PARAM_VERSION.to_string(), vec![self.version.clone()]);
        Ok(())
    }
}

/// How the caller named the agent (§7.2), validated once so the two forms
/// can't disagree further down: an Agent Card reference to resolve, or a
/// single interface to connect to directly.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentSelection {
    Card(String),
    Endpoint { url: String, binding: Binding },
}

fn resolve_agent_selection(cli: &Cli, matches: &ArgMatches) -> Result<AgentSelection, CliError> {
    match (&cli.agent_card, &cli.endpoint) {
        (Some(_), Some(_)) => Err(CliError::InvalidInput(
            "--endpoint and --agent-card are mutually exclusive: --endpoint names an interface \
             directly, so there is no card to resolve"
                .to_string(),
        )),
        // §7.2: with no card to declare the binding, the caller must name
        // exactly one transport — zero leaves the protocol ambiguous, and
        // more than one asks for a preference order over a single interface.
        (None, Some(url)) => match cli.transport.as_slice() {
            [binding] => Ok(AgentSelection::Endpoint {
                url: url.clone(),
                binding: *binding,
            }),
            transports => Err(CliError::InvalidInput(format!(
                "--endpoint requires exactly one --transport (got {}): there is no agent card \
                 to declare the interface's protocol binding",
                transports.len()
            ))),
        },
        (Some(reference), None) => Ok(AgentSelection::Card(reference.clone())),
        (None, None) => Ok(AgentSelection::Card(base_url_card_reference(cli, matches))),
    }
}

/// A one-interface card standing in for the interface `--endpoint` named.
/// Synthesizing keeps transport selection, tenant handling and credential
/// attachment on a single path rather than growing a second card-less one.
fn synthesized_endpoint_card(url: &str, binding: Binding) -> AgentCard {
    AgentCard {
        name: url.to_string(),
        description: "synthesized from --endpoint; no agent card was resolved".to_string(),
        version: VERSION.to_string(),
        supported_interfaces: vec![AgentInterface::new(url, binding.protocol())],
        capabilities: AgentCapabilities::default(),
        default_input_modes: vec![],
        default_output_modes: vec![],
        skills: vec![],
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: None,
        security_requirements: None,
        signatures: None,
    }
}

async fn resolve_client(cli: &Cli, matches: &ArgMatches) -> Result<ResolvedClient, CliError> {
    let card = resolve_agent_card(cli, matches).await?;
    // Re-reading the selection is cheap (it only inspects flags) and avoids
    // a second card fetch: what matters is whether a card was read at all,
    // which `resolve_agent_card` does not report.
    let capabilities = match resolve_agent_selection(cli, matches)? {
        AgentSelection::Endpoint { .. } => DeclaredCapabilities::NoCard,
        AgentSelection::Card(_) => DeclaredCapabilities::Card(card.capabilities.clone()),
    };

    let mut builder = A2AClientFactory::builder();
    if !cli.transport.is_empty() {
        let preferred = cli
            .transport
            .iter()
            .map(|binding| binding.protocol().to_string())
            .collect();
        builder = builder.preferred_bindings(preferred);
    }
    if cli.insecure {
        let insecure = build_insecure_reqwest_client()?;
        builder = builder
            .register(Arc::new(a2a_client::jsonrpc::JsonRpcTransportFactory::new(
                Some(insecure.clone()),
            )))
            .register(Arc::new(a2a_client::rest::RestTransportFactory::new(Some(
                insecure,
            ))));
    }
    // Collected rather than handed to the builder, because the version
    // interceptor can only be built once the selected interface is known
    // (§13.2 negotiates against what *that* interface declares) and
    // `with_interceptors` sets the whole list.
    let mut interceptors: Vec<Arc<dyn a2a_client::middleware::CallInterceptor>> = Vec::new();
    if let Some(token) = &cli.bearer {
        interceptors.push(Arc::new(AuthInterceptor::bearer(token.clone())));
    }
    if let Some(api_key) = &cli.api_key {
        interceptors.push(Arc::new(AuthInterceptor::custom(
            "X-API-Key",
            api_key.clone(),
        )));
    }
    for param in &cli.svc_params {
        interceptors.push(Arc::new(AuthInterceptor::custom(
            param.name.clone(),
            param.value.clone(),
        )));
    }
    if cli.debug {
        interceptors.push(Arc::new(a2a_client::middleware::LoggingInterceptor));
    }

    let factory = builder.build();
    let (client, interface) = factory.create_from_card_with_interface(&card).await?;

    let negotiated = negotiate_a2a_version(
        cli.a2a_version.as_deref(),
        &interface.protocol_version,
        supported_protocol_version(),
    );
    // §13.2: no silent downgrade. Whenever the effective version isn't
    // simply this build's own, say so.
    if let Some(note) = &negotiated.note {
        eprintln!("warning: {note}");
    }
    interceptors.push(Arc::new(VersionInterceptor {
        version: negotiated.version,
    }));

    // Not `cli.tenant.or(interface.tenant)`: A2A §8.3.2 rule 4 and SPEC.md
    // §13.1 require the tenant to be "exactly the value declared in the
    // selected AgentInterface entry", so an explicit --tenant must not
    // displace a declared one. a2a-client applies the declared value itself,
    // overriding what is passed here, so --tenant reaches the wire only
    // where the card declares none (#199).
    //
    // `create_from_card_with_interface` stays: #191's version negotiation
    // needs the selected interface's protocol_version.
    let tenant = cli.tenant.clone();
    Ok(ResolvedClient {
        client: client.with_interceptors(interceptors),
        tenant,
        capabilities,
    })
}

/// An `--agent-card` reference resolved to where the card actually lives
/// (§7.2, §10.1). The three surface forms collapse to two destinations: a
/// local file, or an HTTP(S) URL.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CardReference {
    File(PathBuf),
    Url(String),
}

/// Whether `reference` names a local file rather than a host or URL, using
/// the same rule as the reference implementation: an explicitly
/// path-shaped prefix, or any string that happens to name something on
/// disk. Checked before the host forms so a relative path never gets an
/// `https://` prefix stapled to it.
fn looks_like_file_path(reference: &str) -> bool {
    reference.starts_with('/')
        || reference.starts_with("./")
        || reference.starts_with("../")
        // Windows shapes. Without these a path that doesn't exist *yet*
        // (a typo, a file not written) matches no prefix, falls through to
        // the host forms, and is fetched as `https://C:\…` — so the error
        // reported is "unreachable" rather than "no such card file".
        || reference.starts_with('\\')
        || reference.starts_with(".\\")
        || reference.starts_with("..\\")
        || has_windows_drive_prefix(reference)
        || std::fs::metadata(reference).is_ok()
}

/// Whether `reference` starts with a Windows drive-letter root (`C:\`,
/// `C:/`). The drive letter must be exactly one character before the colon,
/// so a `host:port` is never mistaken for a path — not `localhost:3000`,
/// and not even a single-letter host like `a:3000`, whose third byte is a
/// digit rather than a separator.
fn has_windows_drive_prefix(reference: &str) -> bool {
    let bytes = reference.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Whether `host` (with or without a port) is loopback, which decides the
/// scheme a bare host gets: `http://` for loopback, `https://` otherwise —
/// a local development agent is rarely served over TLS, and defaulting it
/// to `https://` would make the common case fail.
fn is_loopback_host(host: &str) -> bool {
    // An unbracketed IPv6 literal's colons are not port separators, so try
    // the whole reference as an address before splitting on `:` — otherwise
    // `::1` would be truncated to `::` and read as non-loopback.
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return address.is_loopback();
    }

    let host = match host.strip_prefix('[') {
        // Bracketed IPv6, with or without a port: `[::1]`, `[::1]:9000`.
        Some(rest) => rest.split_once(']').map_or(rest, |(inside, _)| inside),
        // Anything else: a trailing `:…` is a port.
        None => host.split_once(':').map_or(host, |(head, _)| head),
    };

    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Normalize an Agent Card reference to the place the card is fetched from
/// (§10.1): a bare host or origin gets the well-known path appended, a full
/// card URL is used as-is, and a local path (`file://…` or plain) resolves
/// to a file on disk.
fn normalize_card_reference(reference: &str) -> CardReference {
    if let Some(rest) = reference.strip_prefix("file://") {
        let path = match rest.strip_prefix('/') {
            // `file:///C:/card.json` is the canonical Windows form: the
            // slash before the drive letter belongs to the URL, not to the
            // path. On Unix there is no drive letter and the leading slash
            // is kept, since it is the root.
            Some(without_slash) if has_windows_drive_prefix(without_slash) => without_slash,
            _ => rest,
        };
        return CardReference::File(PathBuf::from(path));
    }

    if !reference.contains("://") {
        if looks_like_file_path(reference) {
            return CardReference::File(PathBuf::from(reference));
        }
        let scheme = if is_loopback_host(reference) {
            "http"
        } else {
            "https"
        };
        return CardReference::Url(append_well_known_path(&format!("{scheme}://{reference}")));
    }

    CardReference::Url(append_well_known_path(reference))
}

/// A reference that names only a host or origin gets the well-known path;
/// one that already carries a path is a full card URL and is left alone.
fn append_well_known_path(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let has_path = trimmed
        .split_once("://")
        .is_some_and(|(_, rest)| rest.contains('/'));
    if has_path {
        url.to_string()
    } else {
        format!("{trimmed}{WELL_KNOWN_AGENT_CARD_PATH}")
    }
}

/// The reference from the deprecated `--base-url`, used only when
/// `--agent-card` was not given. Warns when `--base-url` was passed
/// explicitly, so the alias is discoverable as deprecated without breaking
/// anything; leaving it at its built-in default is not a deprecated
/// invocation and says nothing.
fn base_url_card_reference(cli: &Cli, matches: &ArgMatches) -> String {
    if matches!(
        matches.value_source("base_url"),
        Some(ValueSource::CommandLine)
    ) {
        eprintln!(
            "warning: --base-url is deprecated; use --agent-card, which also accepts a full \
             card URL or a local file path"
        );
    }
    cli.base_url.clone()
}

async fn resolve_agent_card(cli: &Cli, matches: &ArgMatches) -> Result<AgentCard, CliError> {
    Ok(resolve_agent_card_with_raw(cli, matches).await?.0)
}

/// Like [`resolve_agent_card`], but also returns the raw JSON the card was
/// parsed from.
///
/// `card get --validate` needs those actual bytes rather than the typed
/// `AgentCard`: `AgentCard`'s `Deserialize` has no `deny_unknown_fields`, so
/// an extra property serde tolerated is already gone by the time a typed
/// value exists to re-serialize — exactly the shape of violation §10.1
/// schema validation exists to catch that the type check does not.
async fn resolve_agent_card_with_raw(
    cli: &Cli,
    matches: &ArgMatches,
) -> Result<(AgentCard, Value), CliError> {
    warn_if_insecure_with_credentials(cli);

    let reference = match resolve_agent_selection(cli, matches)? {
        AgentSelection::Card(reference) => reference,
        AgentSelection::Endpoint { url, binding } => {
            let card = synthesized_endpoint_card(&url, binding);
            // Nothing was fetched, so there is nothing "extra" a typed
            // round trip could have lost; the two forms coincide.
            let raw = serde_json::to_value(&card)?;
            return Ok((card, raw));
        }
    };

    match normalize_card_reference(&reference) {
        CardReference::File(path) => read_agent_card_file(&path),
        CardReference::Url(url) => {
            let client = if cli.insecure {
                build_insecure_reqwest_client()?
            } else {
                Client::new()
            };
            let request = apply_request_auth(client.get(&url), cli);
            let response = request.send().await?.error_for_status()?;
            let raw: Value = response.json().await?;
            // Classified the same way the old single-step `response.json::
            // <AgentCard>()` was: a body that parses as JSON but not as the
            // expected shape is CARD_INVALID, not a bare serde_json::Error
            // (-> INTERNAL) now that fetching and typing are two steps.
            let card: AgentCard = serde_json::from_value(raw.clone())
                .map_err(|error| CliError::CardInvalid(format!("{url}: {error}")))?;
            Ok((card, raw))
        }
    }
}

/// Read an Agent Card from disk, classified per Appendix D: a file that
/// isn't there or can't be read is `CARD_NOT_FOUND` (the card was not
/// found where the caller pointed), while a file that is there but isn't a
/// card is `CARD_INVALID` — the same split the HTTP path makes between a
/// non-2xx response and a body that won't deserialize.
fn read_agent_card_file(path: &Path) -> Result<(AgentCard, Value), CliError> {
    let text = std::fs::read_to_string(path).map_err(|source| CliError::CardFile {
        path: path.display().to_string(),
        source,
    })?;
    let raw: Value = serde_json::from_str(&text)
        .map_err(|error| CliError::CardInvalid(format!("{}: {error}", path.display())))?;
    let card: AgentCard = serde_json::from_value(raw.clone())
        .map_err(|error| CliError::CardInvalid(format!("{}: {error}", path.display())))?;
    Ok((card, raw))
}

fn apply_request_auth(mut request: RequestBuilder, cli: &Cli) -> RequestBuilder {
    if let Some(token) = &cli.bearer {
        request = request.bearer_auth(token);
    }
    if let Some(api_key) = &cli.api_key {
        request = request.header("X-API-Key", api_key);
    }
    for param in &cli.svc_params {
        request = request.header(&param.name, &param.value);
    }
    request
}

/// `--insecure` disables TLS certificate verification (development only).
#[cfg(any(feature = "rustls-tls", feature = "native-tls"))]
fn build_insecure_reqwest_client() -> Result<Client, CliError> {
    Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(CliError::Http)
}

/// No TLS backend compiled in: there is no certificate verification to
/// disable, so `--insecure` is a no-op rather than a build error.
#[cfg(not(any(feature = "rustls-tls", feature = "native-tls")))]
fn build_insecure_reqwest_client() -> Result<Client, CliError> {
    Client::builder().build().map_err(CliError::Http)
}

/// §12.1/AUTH_003: never disable TLS verification silently. Warns whenever
/// `--insecure` is set, and names the specific risk when a credential would
/// also be sent over that connection.
fn warn_if_insecure_with_credentials(cli: &Cli) {
    if !cli.insecure {
        return;
    }
    let sends_credentials = cli.bearer.is_some() || cli.api_key.is_some();
    if sends_credentials {
        eprintln!(
            "warning: --insecure disables TLS certificate verification, and a credential \
             (--bearer/--api-key) is being sent over this connection — use only against a \
             trusted development endpoint"
        );
    } else {
        eprintln!(
            "warning: --insecure disables TLS certificate verification — use only against a \
             trusted development endpoint"
        );
    }
}

/// Renders `text` mode (§11.2): one `Label: value` field per line, stable
/// labels across invocations, no terminal control sequences. Content the
/// field form can't carry on one line (a rendered artifact, a formatted data
/// part) is a *block*: its own `Label:` line, the content, then a blank
/// line, never interleaved with field lines.
trait TextRender {
    fn render_text(&self) -> String;
}

/// Accumulates field lines and blocks for [`TextRender`] impls, then joins
/// them with a single trailing newline trimmed off (`println!` adds it back).
#[derive(Default)]
struct TextOutput {
    lines: Vec<String>,
}

impl TextOutput {
    fn field(&mut self, label: &str, value: impl std::fmt::Display) -> &mut Self {
        self.lines.push(format!("{label}: {value}"));
        self
    }

    fn raw(&mut self, line: impl Into<String>) -> &mut Self {
        self.lines.push(line.into());
        self
    }

    /// A block: its own `Label:` line, `content` (each of its lines emitted
    /// as-is), then a blank line closing it.
    fn block(&mut self, label: &str, content: &str) -> &mut Self {
        self.lines.push(format!("{label}:"));
        self.lines
            .extend(content.lines().map(|line| line.to_string()));
        self.lines.push(String::new());
        self
    }

    fn finish(self) -> String {
        let mut text = self.lines.join("\n");
        while text.ends_with('\n') {
            text.pop();
        }
        text
    }
}

/// Short-form label for a task state (§9.1); the wire form
/// (`TASK_STATE_COMPLETED`, ...) is what `-o json` emits, not this.
fn task_state_label(state: &TaskState) -> &'static str {
    match state {
        TaskState::Unspecified => "UNSPECIFIED",
        TaskState::Submitted => "SUBMITTED",
        TaskState::Working => "WORKING",
        TaskState::Completed => "COMPLETED",
        TaskState::Failed => "FAILED",
        TaskState::Canceled => "CANCELED",
        TaskState::InputRequired => "INPUT_REQUIRED",
        TaskState::Rejected => "REJECTED",
        TaskState::AuthRequired => "AUTH_REQUIRED",
    }
}

/// Render one message part: a text part as readable text, a data part as
/// formatted JSON, a file part by name/media type/size (§10.2, §10.3) —
/// never dumped as raw structure, and never silently discarded.
fn render_part(out: &mut TextOutput, part: &Part) {
    match &part.content {
        PartContent::Text(text) => {
            out.block("Text", text);
        }
        PartContent::Data(value) => {
            let pretty = serde_json::to_string_pretty(value)
                .unwrap_or_else(|_| serde_json::Value::Null.to_string());
            out.block("Data", &pretty);
        }
        PartContent::Raw(bytes) => {
            let name = part.filename.as_deref().unwrap_or("(unnamed)");
            match &part.media_type {
                Some(media_type) => out.field(
                    "File",
                    format!("{name} ({media_type}, {} bytes)", bytes.len()),
                ),
                None => out.field("File", format!("{name} ({} bytes)", bytes.len())),
            };
        }
        PartContent::Url(url) => {
            match &part.media_type {
                Some(media_type) => out.field("File", format!("{url} ({media_type})")),
                None => out.field("File", url),
            };
        }
    }
}

fn render_message_parts(out: &mut TextOutput, message: &Message) {
    if message.parts.is_empty() {
        out.field("Parts", "(none)");
        return;
    }
    for part in &message.parts {
        render_part(out, part);
    }
}

impl TextRender for AgentCard {
    fn render_text(&self) -> String {
        let mut out = TextOutput::default();
        out.field("Name", &self.name)
            .field("Description", &self.description)
            .field("Version", &self.version)
            .field("Streaming", self.capabilities.streaming.unwrap_or(false))
            .field(
                "Push Notifications",
                self.capabilities.push_notifications.unwrap_or(false),
            )
            .field(
                "Extended Card",
                self.capabilities.extended_agent_card.unwrap_or(false),
            );

        let interfaces = if self.supported_interfaces.is_empty() {
            "  (none)".to_string()
        } else {
            self.supported_interfaces
                .iter()
                .map(|interface| {
                    format!(
                        "  - {} {} (v{})",
                        interface.protocol_binding, interface.url, interface.protocol_version
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        out.block("Interfaces", &interfaces);
        out.field("Skills", self.skills.len());
        out.finish()
    }
}

impl TextRender for Task {
    fn render_text(&self) -> String {
        let mut out = TextOutput::default();
        out.field("Task ID", &self.id)
            .field("Context ID", &self.context_id)
            .field("State", task_state_label(&self.status.state));

        if let Some(message) = &self.status.message {
            render_message_parts(&mut out, message);
        }

        if let Some(artifacts) = &self.artifacts {
            for artifact in artifacts {
                let label = artifact.name.as_deref().unwrap_or(&artifact.artifact_id);
                out.raw(format!("Artifact: {label}"));
                for part in &artifact.parts {
                    render_part(&mut out, part);
                }
            }
        }

        if matches!(
            self.status.state,
            TaskState::InputRequired | TaskState::AuthRequired
        ) {
            out.field(
                "Resume with",
                format!("a2acli send --task-id {} \"<reply>\"", self.id),
            );
        }

        out.finish()
    }
}

impl TextRender for Message {
    fn render_text(&self) -> String {
        let mut out = TextOutput::default();
        if let Some(context_id) = &self.context_id {
            out.field("Context ID", context_id);
        }
        if let Some(task_id) = &self.task_id {
            out.field("Task ID", task_id);
        }
        render_message_parts(&mut out, self);
        out.finish()
    }
}

impl TextRender for SendMessageResponse {
    fn render_text(&self) -> String {
        match self {
            SendMessageResponse::Task(task) => task.render_text(),
            SendMessageResponse::Message(message) => message.render_text(),
        }
    }
}

impl TextRender for ListTasksResponse {
    fn render_text(&self) -> String {
        let mut out = TextOutput::default();
        out.field("Total", self.total_size)
            .field("Page Size", self.page_size);
        if !self.next_page_token.is_empty() {
            out.field("Next Page Token", &self.next_page_token);
        }
        if self.tasks.is_empty() {
            out.field("Tasks", "(none)");
        } else {
            for task in &self.tasks {
                out.block("Task", &task.render_text());
            }
        }
        out.finish()
    }
}

impl TextRender for TaskPushNotificationConfig {
    fn render_text(&self) -> String {
        let mut out = TextOutput::default();
        if let Some(id) = &self.id {
            out.field("Config ID", id);
        }
        out.field("Task ID", &self.task_id).field("URL", &self.url);
        if let Some(token) = &self.token {
            out.field("Token", token);
        }
        if let Some(auth) = &self.authentication {
            out.field("Auth Scheme", &auth.scheme);
        }
        out.finish()
    }
}

impl TextRender for ListTaskPushNotificationConfigsResponse {
    fn render_text(&self) -> String {
        let mut out = TextOutput::default();
        if let Some(token) = &self.next_page_token {
            out.field("Next Page Token", token);
        }
        if self.configs.is_empty() {
            out.field("Configs", "(none)");
        } else {
            for config in &self.configs {
                out.block("Push Config", &config.render_text());
            }
        }
        out.finish()
    }
}

/// The result of `task push-config delete`, replacing an ad hoc
/// `serde_json::json!` value so it can carry both a `-o json` shape and a
/// `-o text` rendering like every other response type.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PushConfigDeleted {
    deleted: bool,
    task_id: String,
    id: String,
}

impl TextRender for PushConfigDeleted {
    fn render_text(&self) -> String {
        let mut out = TextOutput::default();
        out.field("Deleted", self.deleted)
            .field("Task ID", &self.task_id)
            .field("Config ID", &self.id);
        out.finish()
    }
}

impl TextRender for StreamResponse {
    fn render_text(&self) -> String {
        match self {
            StreamResponse::Task(task) => task.render_text(),
            StreamResponse::Message(message) => message.render_text(),
            StreamResponse::StatusUpdate(event) => {
                let mut out = TextOutput::default();
                out.field("Task ID", &event.task_id)
                    .field("Context ID", &event.context_id)
                    .field("State", task_state_label(&event.status.state));
                if matches!(
                    event.status.state,
                    TaskState::InputRequired | TaskState::AuthRequired
                ) {
                    out.field(
                        "Resume with",
                        format!("a2acli send --task-id {} \"<reply>\"", event.task_id),
                    );
                }
                out.finish()
            }
            StreamResponse::ArtifactUpdate(event) => {
                let mut out = TextOutput::default();
                out.field("Task ID", &event.task_id)
                    .field("Context ID", &event.context_id);
                let label = event
                    .artifact
                    .name
                    .as_deref()
                    .unwrap_or(&event.artifact.artifact_id);
                out.raw(format!("Artifact: {label}"));
                for part in &event.artifact.parts {
                    render_part(&mut out, part);
                }
                out.finish()
            }
        }
    }
}

/// Print `value` in the caller-selected format: `text` (default, §11.2) or
/// `-o json` as one pretty/compact document per §11.3 (never JSONL — that
/// form is only used by [`consume_stream`] under `--stream`).
fn print_output<T: Serialize + TextRender>(value: &T, cli: &Cli) -> Result<(), CliError> {
    match cli.output {
        OutputFormat::Text => {
            println!("{}", value.render_text());
            Ok(())
        }
        OutputFormat::Json => print_json(value, cli.compact),
    }
}

fn print_json<T: Serialize>(value: &T, compact: bool) -> Result<(), CliError> {
    if compact {
        println!("{}", serde_json::to_string(value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

fn parse_header(input: &str) -> Result<HeaderArg, String> {
    let (name, value) = input
        .split_once(':')
        .ok_or_else(|| "header must be in NAME:VALUE format".to_string())?;

    let name = name.trim();
    let value = value.trim();

    if name.is_empty() {
        return Err("header name cannot be empty".to_string());
    }

    Ok(HeaderArg {
        name: name.to_string(),
        value: value.to_string(),
    })
}

async fn finish_client_call<T: a2a_client::Transport, V>(
    client: A2AClient<T>,
    result: Result<V, A2AError>,
) -> Result<V, CliError> {
    match result {
        Ok(value) => {
            client.destroy().await?;
            Ok(value)
        }
        Err(error) => {
            let _ = client.destroy().await;
            Err(error.into())
        }
    }
}

/// Prints one streamed value per §11.1/§11.2's output-mode rules.
fn print_stream_value<V: Serialize + TextRender>(value: &V, cli: &Cli) -> Result<(), CliError> {
    match cli.output {
        OutputFormat::Text => {
            println!("{}", value.render_text());
            Ok(())
        }
        OutputFormat::Json => serde_json::to_string(value)
            .map(|line| println!("{line}"))
            .map_err(CliError::from),
    }
}

/// Warns, prints, and destroys `client` first if printing fails.
async fn print_or_destroy<T: a2a_client::Transport, V: Serialize + TextRender + TaskOutcome>(
    client: &A2AClient<T>,
    value: &V,
    cli: &Cli,
) -> Result<(), CliError> {
    value.warn_outcome();
    if let Err(error) = print_stream_value(value, cli) {
        let _ = client.destroy().await;
        return Err(error);
    }
    Ok(())
}

/// Consumes a streamed event sequence to completion, printing each event.
/// Used by `send --stream`, which does not reconnect -- see
/// [`subscribe_with_resumption`] for `task subscribe`, which does.
async fn consume_stream<T: a2a_client::Transport, V: Serialize + TextRender + TaskOutcome>(
    client: A2AClient<T>,
    mut stream: BoxStream<'static, Result<V, A2AError>>,
    cli: &Cli,
) -> Result<(), CliError> {
    loop {
        match stream.next().await {
            Some(Ok(value)) => {
                print_or_destroy(&client, &value, cli).await?;
            }
            Some(Err(error)) => {
                let _ = client.destroy().await;
                return Err(error.into());
            }
            None => {
                client.destroy().await?;
                return Ok(());
            }
        }
    }
}

/// The task state a `StreamResponse` carries, if any.
fn stream_response_state(value: &StreamResponse) -> Option<TaskState> {
    match value {
        StreamResponse::Task(task) => Some(task.status.state.clone()),
        StreamResponse::StatusUpdate(event) => Some(event.status.state.clone()),
        StreamResponse::Message(_) | StreamResponse::ArtifactUpdate(_) => None,
    }
}

/// `task subscribe` (§9.4, `A2ACLI_TASK_SUBSCRIBE_002`): reconnects when the
/// stream ends before the task settles, using the last state observed to
/// tell a cut from a finish. The reconnect's first event reconciles state
/// (no `task get` needed) and is suppressed if unchanged from before the
/// cut. `--timeout` bounds reconnection cumulatively, not a healthy stream.
async fn subscribe_with_resumption<T: a2a_client::Transport>(
    client: A2AClient<T>,
    mut stream: BoxStream<'static, Result<StreamResponse, A2AError>>,
    task_id: String,
    tenant: Option<String>,
    cli: &Cli,
) -> Result<(), CliError> {
    let deadline = tokio::time::Instant::now() + cli.timeout;
    let mut last_state: Option<TaskState> = None;
    let mut attempt: u32 = 0;

    loop {
        let state_before_this_attempt = last_state.clone();
        let mut first_event = attempt > 0;

        loop {
            match stream.next().await {
                Some(Ok(value)) => {
                    let state = stream_response_state(&value);
                    if let Some(state) = &state {
                        last_state = Some(state.clone());
                    }

                    let is_unchanged_reconciliation =
                        first_event && state.is_some() && state == state_before_this_attempt;
                    first_event = false;
                    if is_unchanged_reconciliation {
                        continue;
                    }

                    print_or_destroy(&client, &value, cli).await?;
                }
                Some(Err(error)) => {
                    let _ = client.destroy().await;
                    return Err(error.into());
                }
                None => break,
            }
        }

        if last_state.as_ref().is_some_and(is_settled) {
            client.destroy().await?;
            return Ok(());
        }

        // Retries both a cut and a failed re-subscribe under one budget.
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                let _ = client.destroy().await;
                return Err(CliError::Timeout {
                    task_id: task_id.clone(),
                    timeout: cli.timeout,
                });
            }
            attempt += 1;
            eprintln!(
                "warning: subscription to task {task_id} was interrupted before it settled; \
                 reconnecting (attempt {attempt})..."
            );
            tokio::time::sleep(
                cli.poll_interval
                    .min(deadline.saturating_duration_since(now)),
            )
            .await;

            match client
                .subscribe_to_task(&SubscribeToTaskRequest {
                    id: task_id.clone(),
                    tenant: tenant.clone(),
                })
                .await
            {
                Ok(new_stream) => {
                    stream = new_stream;
                    break;
                }
                Err(_) => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a_client::{ServiceParams, Transport};
    use a2a_server::jsonrpc::jsonrpc_router;
    use a2a_server::rest::rest_router;
    use a2a_server::{
        RequestHandler, ServiceParams as HandlerServiceParams, WELL_KNOWN_AGENT_CARD_PATH,
    };
    use async_trait::async_trait;
    use axum::routing::get;
    use axum::{Json, Router};
    use futures::stream;
    use reqwest::header;
    use serde::ser;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    struct TestTransport {
        destroy_error: Option<A2AError>,
    }

    #[async_trait]
    impl Transport for TestTransport {
        async fn send_message(
            &self,
            _params: &ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn send_streaming_message(
            &self,
            _params: &ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn get_task(
            &self,
            _params: &ServiceParams,
            _req: &GetTaskRequest,
        ) -> Result<Task, A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn list_tasks(
            &self,
            _params: &ServiceParams,
            _req: &ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn cancel_task(
            &self,
            _params: &ServiceParams,
            _req: &CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn subscribe_to_task(
            &self,
            _params: &ServiceParams,
            _req: &SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn create_push_config(
            &self,
            _params: &ServiceParams,
            _req: &TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn get_push_config(
            &self,
            _params: &ServiceParams,
            _req: &GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn list_push_configs(
            &self,
            _params: &ServiceParams,
            _req: &ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn delete_push_config(
            &self,
            _params: &ServiceParams,
            _req: &DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn get_extended_agent_card(
            &self,
            _params: &ServiceParams,
            _req: &GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            Err(A2AError::unsupported_operation("unused"))
        }

        async fn destroy(&self) -> Result<(), A2AError> {
            match &self.destroy_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    struct FailingSerialize;

    impl Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(ser::Error::custom("serialize failed"))
        }
    }

    impl TextRender for FailingSerialize {
        fn render_text(&self) -> String {
            String::new()
        }
    }

    impl TaskOutcome for FailingSerialize {
        fn warn_outcome(&self) {}
    }

    fn make_test_client(destroy_error: Option<A2AError>) -> A2AClient<TestTransport> {
        A2AClient::new(TestTransport { destroy_error })
    }

    #[derive(Default)]
    struct RunTestState {
        tasks: Mutex<BTreeMap<String, Task>>,
        push_configs: Mutex<BTreeMap<(String, String), TaskPushNotificationConfig>>,
    }

    struct RunTestHandler {
        state: Arc<RunTestState>,
        extended_card: AgentCard,
    }

    struct RunTestServer {
        base_url: String,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for RunTestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    impl RunTestServer {
        async fn spawn() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let state = Arc::new(RunTestState::default());

            state.tasks.lock().unwrap().insert(
                "task-1".to_string(),
                make_fixture_task("task-1", "ctx-1", TaskState::Completed, "seeded result"),
            );

            let public_card = make_fixture_card(&base_url, "Fixture Agent");
            let extended_card = make_fixture_card(&base_url, "Fixture Agent (extended)");
            let handler = Arc::new(RunTestHandler {
                state,
                extended_card,
            });

            let card = public_card.clone();
            let app = Router::new()
                .route(
                    WELL_KNOWN_AGENT_CARD_PATH,
                    get(move || {
                        let card = card.clone();
                        async move { Json(card) }
                    }),
                )
                .nest("/jsonrpc", jsonrpc_router(handler.clone()))
                .nest("/rest", rest_router(handler));

            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            RunTestServer { base_url, handle }
        }
    }

    #[async_trait]
    impl RequestHandler for RunTestHandler {
        async fn send_message(
            &self,
            _params: &HandlerServiceParams,
            req: SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            let task_id = req
                .message
                .task_id
                .clone()
                .unwrap_or_else(|| "task-send".to_string());
            let context_id = req
                .message
                .context_id
                .clone()
                .unwrap_or_else(|| "ctx-send".to_string());
            let text = req.message.text().unwrap_or_default();
            if text == "send-error" {
                return Err(A2AError::invalid_request("send failed"));
            }

            let task = make_fixture_task(
                &task_id,
                &context_id,
                TaskState::Completed,
                &format!("Echo: {text}"),
            );
            self.state
                .tasks
                .lock()
                .unwrap()
                .insert(task_id, task.clone());
            Ok(SendMessageResponse::Task(task))
        }

        async fn send_streaming_message(
            &self,
            _params: &HandlerServiceParams,
            req: SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            let task_id = req
                .message
                .task_id
                .clone()
                .unwrap_or_else(|| "task-stream".to_string());
            let context_id = req
                .message
                .context_id
                .clone()
                .unwrap_or_else(|| "ctx-stream".to_string());
            let text = req.message.text().unwrap_or_default();
            if text == "stream-error" {
                return Ok(Box::pin(stream::once(async {
                    Err(A2AError::internal("stream failed"))
                })));
            }

            let task = make_fixture_task(
                &task_id,
                &context_id,
                TaskState::Completed,
                &format!("Echo: {text}"),
            );
            self.state
                .tasks
                .lock()
                .unwrap()
                .insert(task_id.clone(), task.clone());

            Ok(Box::pin(stream::iter(vec![
                Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id,
                    context_id,
                    status: TaskStatus {
                        state: TaskState::Working,
                        message: None,
                        timestamp: None,
                    },
                    metadata: None,
                })),
                Ok(StreamResponse::Task(task)),
            ])))
        }

        async fn get_task(
            &self,
            _params: &HandlerServiceParams,
            req: GetTaskRequest,
        ) -> Result<Task, A2AError> {
            self.state
                .tasks
                .lock()
                .unwrap()
                .get(&req.id)
                .cloned()
                .ok_or_else(|| A2AError::task_not_found(&req.id))
        }

        async fn list_tasks(
            &self,
            _params: &HandlerServiceParams,
            req: ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            if req.context_id.as_deref() == Some("error") {
                return Err(A2AError::invalid_params("list failed"));
            }

            let tasks = self
                .state
                .tasks
                .lock()
                .unwrap()
                .values()
                .filter(|task| {
                    req.context_id
                        .as_ref()
                        .map(|context_id| task.context_id == *context_id)
                        .unwrap_or(true)
                })
                .filter(|task| {
                    req.status
                        .as_ref()
                        .map(|status| task.status.state == *status)
                        .unwrap_or(true)
                })
                .cloned()
                .collect();

            Ok(ListTasksResponse {
                tasks,
                next_page_token: String::new(),
                page_size: 0,
                total_size: 0,
            })
        }

        async fn cancel_task(
            &self,
            _params: &HandlerServiceParams,
            req: CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            let mut tasks = self.state.tasks.lock().unwrap();
            let task = tasks
                .get(&req.id)
                .cloned()
                .ok_or_else(|| A2AError::task_not_found(&req.id))?;
            let canceled =
                make_fixture_task(&task.id, &task.context_id, TaskState::Canceled, "canceled");
            tasks.insert(req.id, canceled.clone());
            Ok(canceled)
        }

        async fn subscribe_to_task(
            &self,
            _params: &HandlerServiceParams,
            req: SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            if req.id == "stream-error" {
                return Ok(Box::pin(stream::once(async {
                    Err(A2AError::internal("stream failed"))
                })));
            }

            let task = self
                .state
                .tasks
                .lock()
                .unwrap()
                .get(&req.id)
                .cloned()
                .ok_or_else(|| A2AError::task_not_found(&req.id))?;

            Ok(Box::pin(stream::iter(vec![
                Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: req.id.clone(),
                    context_id: task.context_id.clone(),
                    status: TaskStatus {
                        state: TaskState::Working,
                        message: None,
                        timestamp: None,
                    },
                    metadata: None,
                })),
                Ok(StreamResponse::Task(task)),
            ])))
        }

        async fn create_push_config(
            &self,
            _params: &HandlerServiceParams,
            req: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            if !self.state.tasks.lock().unwrap().contains_key(&req.task_id) {
                return Err(A2AError::task_not_found(&req.task_id));
            }

            let config_id = req.id.clone().unwrap_or_else(|| "generated".to_string());
            self.state
                .push_configs
                .lock()
                .unwrap()
                .insert((req.task_id.clone(), config_id), req.clone());
            Ok(req)
        }

        async fn get_push_config(
            &self,
            _params: &HandlerServiceParams,
            req: GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.state
                .push_configs
                .lock()
                .unwrap()
                .get(&(req.task_id.clone(), req.id.clone()))
                .cloned()
                .ok_or_else(|| A2AError::task_not_found(&req.task_id))
        }

        async fn list_push_configs(
            &self,
            _params: &HandlerServiceParams,
            req: ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            if req.task_id == "missing" {
                return Err(A2AError::task_not_found(&req.task_id));
            }

            let configs = self
                .state
                .push_configs
                .lock()
                .unwrap()
                .values()
                .filter(|config| config.task_id == req.task_id)
                .cloned()
                .collect();

            Ok(ListTaskPushNotificationConfigsResponse {
                configs,
                next_page_token: None,
            })
        }

        async fn delete_push_config(
            &self,
            _params: &HandlerServiceParams,
            req: DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            let deleted = self
                .state
                .push_configs
                .lock()
                .unwrap()
                .remove(&(req.task_id.clone(), req.id.clone()));
            if deleted.is_none() {
                return Err(A2AError::task_not_found(&req.task_id));
            }
            Ok(())
        }

        async fn get_extended_agent_card(
            &self,
            _params: &HandlerServiceParams,
            req: GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            if req.tenant.as_deref() == Some("error") {
                return Err(A2AError::unsupported_operation("extended card denied"));
            }

            Ok(self.extended_card.clone())
        }
    }

    fn make_fixture_card(base_url: &str, name: &str) -> AgentCard {
        AgentCard {
            name: name.to_string(),
            description: "CLI unit-test fixture".to_string(),
            version: VERSION.to_string(),
            supported_interfaces: vec![
                AgentInterface::new(format!("{base_url}/jsonrpc"), TRANSPORT_PROTOCOL_JSONRPC),
                AgentInterface::new(format!("{base_url}/rest"), TRANSPORT_PROTOCOL_HTTP_JSON),
            ],
            capabilities: AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(true),
                extensions: None,
                extended_agent_card: Some(true),
            },
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: vec![],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        }
    }

    fn make_fixture_task(task_id: &str, context_id: &str, state: TaskState, text: &str) -> Task {
        Task {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            status: TaskStatus {
                state,
                message: Some(Message {
                    message_id: format!("msg-{task_id}"),
                    context_id: Some(context_id.to_string()),
                    task_id: Some(task_id.to_string()),
                    role: Role::Agent,
                    parts: vec![Part::text(text)],
                    metadata: None,
                    extensions: None,
                    reference_task_ids: None,
                }),
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    fn build_args(base_url: &str, args: &[&str]) -> Vec<String> {
        let mut argv = vec![
            "a2acli".to_string(),
            "--agent-card".to_string(),
            base_url.to_string(),
        ];
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        argv
    }

    /// Parse and run `a2acli` exactly as the real binary does, so the
    /// `ArgMatches`-derived part ordering (`resolve_message_parts`) is
    /// exercised the same way in tests as in production.
    async fn run_test_cli(base_url: &str, args: &[&str]) -> Result<(), CliError> {
        run_args(build_args(base_url, args)).await
    }

    async fn unused_base_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}")
    }

    fn assert_unsupported_operation<T>(result: Result<T, A2AError>) {
        match result {
            Ok(_) => panic!("expected unsupported operation error"),
            Err(err) => assert_eq!(err.code, a2a::error_code::UNSUPPORTED_OPERATION),
        }
    }

    #[test]
    fn test_parse_header() {
        let header = parse_header("Authorization: Bearer token").unwrap();
        assert_eq!(header.name, "Authorization");
        assert_eq!(header.value, "Bearer token");
    }

    #[test]
    fn test_parse_header_requires_separator() {
        let err = parse_header("Authorization").unwrap_err();
        assert_eq!(err, "header must be in NAME:VALUE format");
    }

    #[test]
    fn test_parse_header_requires_name() {
        let err = parse_header(": value").unwrap_err();
        assert_eq!(err, "header name cannot be empty");
    }

    #[test]
    fn test_build_send_message_request_populates_optional_fields() {
        let request = build_send_message_request(
            &MessageCommand {
                text: Some("hello".to_string()),
                text_parts: Vec::new(),
                file_parts: Vec::new(),
                data_parts: Vec::new(),
                media_types: Vec::new(),
                context_id: Some("ctx-1".to_string()),
                task_id: Some("task-1".to_string()),
                history_length: Some(4),
                accepted_output_modes: vec!["text/plain".to_string()],
                return_immediately: true,
                stream: false,
            },
            vec![Part::text("hello")],
            Some("tenant-1".to_string()),
        );

        assert_eq!(request.message.text(), Some("hello"));
        assert_eq!(request.message.context_id.as_deref(), Some("ctx-1"));
        assert_eq!(request.message.task_id.as_deref(), Some("task-1"));
        assert_eq!(request.tenant.as_deref(), Some("tenant-1"));
        assert_eq!(
            request
                .configuration
                .as_ref()
                .and_then(|config| config.history_length),
            Some(4)
        );
        assert_eq!(
            request
                .configuration
                .as_ref()
                .and_then(|config| config.return_immediately),
            Some(true)
        );
    }

    #[test]
    fn test_build_send_message_request_without_optional_fields() {
        let request = build_send_message_request(
            &MessageCommand {
                text: Some("hello".to_string()),
                text_parts: Vec::new(),
                file_parts: Vec::new(),
                data_parts: Vec::new(),
                media_types: Vec::new(),
                context_id: None,
                task_id: None,
                history_length: None,
                accepted_output_modes: Vec::new(),
                return_immediately: false,
                stream: false,
            },
            vec![Part::text("hello")],
            None,
        );

        assert_eq!(request.message.text(), Some("hello"));
        assert!(request.configuration.is_none());
        assert!(request.tenant.is_none());
    }

    #[test]
    fn test_cli_parse_send_command() {
        let cli = Cli::try_parse_from([
            "a2acli",
            "--transport",
            "jsonrpc",
            "--svc-param",
            "X-Test:123",
            "send",
            "hello",
            "--history-length",
            "2",
        ])
        .unwrap();

        assert_eq!(cli.transport, vec![Binding::Jsonrpc]);
        assert_eq!(cli.svc_params.len(), 1);
        assert!(matches!(cli.command, Command::Send(_)));
    }

    #[test]
    fn test_cli_parse_push_config_create_command() {
        let cli = Cli::try_parse_from([
            "a2acli",
            "task",
            "push-config",
            "create",
            "task-1",
            "https://example.com/callback",
            "--config-id",
            "cfg-1",
            "--token",
            "tok-1",
            "--auth-scheme",
            "Bearer",
            "--auth-credentials",
            "secret",
        ])
        .unwrap();

        match cli.command {
            Command::Task {
                command:
                    TaskCommand::PushConfig {
                        command: PushConfigCommand::Create(command),
                    },
            } => {
                assert_eq!(command.task_id, "task-1");
                assert_eq!(command.url, "https://example.com/callback");
                assert_eq!(command.config_id.as_deref(), Some("cfg-1"));
                assert_eq!(command.token.as_deref(), Some("tok-1"));
                assert_eq!(command.auth_scheme.as_deref(), Some("Bearer"));
                assert_eq!(command.auth_credentials.as_deref(), Some("secret"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn test_build_push_notification_config() {
        let config = build_push_notification_config(&CreatePushConfigCommand {
            task_id: "task-1".to_string(),
            url: "https://example.com/callback".to_string(),
            config_id: Some("cfg-1".to_string()),
            token: Some("tok-1".to_string()),
            auth_scheme: Some("Bearer".to_string()),
            auth_credentials: Some("secret".to_string()),
        })
        .unwrap();

        assert_eq!(config.id.as_deref(), Some("cfg-1"));
        assert_eq!(config.token.as_deref(), Some("tok-1"));
        assert_eq!(
            config
                .authentication
                .as_ref()
                .map(|auth| auth.scheme.as_str()),
            Some("Bearer")
        );
        assert_eq!(
            config
                .authentication
                .as_ref()
                .and_then(|auth| auth.credentials.as_deref()),
            Some("secret")
        );
    }

    #[test]
    fn test_build_push_notification_config_requires_auth_scheme() {
        let err = build_push_notification_config(&CreatePushConfigCommand {
            task_id: "task-1".to_string(),
            url: "https://example.com/callback".to_string(),
            config_id: None,
            token: None,
            auth_scheme: None,
            auth_credentials: Some("secret".to_string()),
        })
        .unwrap_err();

        assert!(matches!(err, CliError::InvalidInput(_)));
    }

    #[test]
    fn test_build_push_notification_config_without_authentication() {
        let config = build_push_notification_config(&CreatePushConfigCommand {
            task_id: "task-1".to_string(),
            url: "https://example.com/callback".to_string(),
            config_id: None,
            token: None,
            auth_scheme: None,
            auth_credentials: None,
        })
        .unwrap();

        assert_eq!(config.url, "https://example.com/callback");
        assert!(config.authentication.is_none());
    }

    #[test]
    fn test_binding_protocols() {
        assert_eq!(Binding::Jsonrpc.protocol(), TRANSPORT_PROTOCOL_JSONRPC);
        assert_eq!(Binding::Rest.protocol(), TRANSPORT_PROTOCOL_HTTP_JSON);
    }

    #[test]
    fn test_apply_request_auth_builds_headers() {
        let cli = Cli::try_parse_from([
            "a2acli",
            "--bearer",
            "secret",
            "--api-key",
            "key-123",
            "--svc-param",
            "X-Test: 123",
            "card",
            "get",
        ])
        .unwrap();

        let request = apply_request_auth(Client::new().get("http://example.com"), &cli)
            .build()
            .unwrap();

        assert_eq!(
            request.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer secret"
        );
        assert_eq!(request.headers().get("X-API-Key").unwrap(), "key-123");
        assert_eq!(request.headers().get("X-Test").unwrap(), "123");
    }

    #[tokio::test]
    async fn test_finish_client_call_propagates_destroy_error() {
        let err = finish_client_call(
            make_test_client(Some(A2AError::internal("destroy failed"))),
            Ok(serde_json::json!({ "ok": true })),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, CliError::A2A(_)));
    }

    fn json_cli() -> Cli {
        parse_with_matches(&["--output", "json", "task", "get", "unused"]).0
    }

    #[tokio::test]
    async fn test_consume_stream_reports_json_error() {
        let stream = Box::pin(stream::once(async { Ok(FailingSerialize) }));
        let err = consume_stream(make_test_client(None), stream, &json_cli())
            .await
            .unwrap_err();

        assert!(matches!(err, CliError::Json(_)));
    }

    /// `print_or_destroy` is the print-failure path both `consume_stream`
    /// and `subscribe_with_resumption` share; exercised directly since
    /// `StreamResponse`'s own `Serialize` impl never fails for a real
    /// value, so this path is otherwise unreachable through
    /// `subscribe_with_resumption` specifically.
    #[tokio::test]
    async fn test_print_or_destroy_reports_json_error() {
        let client = make_test_client(None);
        let err = print_or_destroy(&client, &FailingSerialize, &json_cli())
            .await
            .unwrap_err();

        assert!(matches!(err, CliError::Json(_)));
    }

    #[tokio::test]
    async fn test_print_or_destroy_succeeds_in_text_mode() {
        let client = make_test_client(None);
        print_or_destroy(&client, &FailingSerialize, &text_cli())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_consume_stream_propagates_destroy_error_on_completion() {
        let stream = Box::pin(stream::empty::<Result<Task, A2AError>>());
        let err = consume_stream(
            make_test_client(Some(A2AError::internal("destroy failed"))),
            stream,
            &json_cli(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, CliError::A2A(_)));
    }

    #[tokio::test]
    async fn test_test_transport_methods_return_unused_errors() {
        let transport = TestTransport {
            destroy_error: None,
        };
        let params = ServiceParams::new();

        assert_unsupported_operation(
            transport
                .send_message(
                    &params,
                    &SendMessageRequest {
                        message: Message::new(Role::User, vec![Part::text("hello")]),
                        configuration: None,
                        metadata: None,
                        tenant: None,
                    },
                )
                .await,
        );
        assert_unsupported_operation(
            transport
                .send_streaming_message(
                    &params,
                    &SendMessageRequest {
                        message: Message::new(Role::User, vec![Part::text("hello")]),
                        configuration: None,
                        metadata: None,
                        tenant: None,
                    },
                )
                .await,
        );
        assert_unsupported_operation(
            transport
                .get_task(
                    &params,
                    &GetTaskRequest {
                        id: "task-1".to_string(),
                        history_length: None,
                        tenant: None,
                    },
                )
                .await,
        );
        assert_unsupported_operation(
            transport
                .list_tasks(
                    &params,
                    &ListTasksRequest {
                        context_id: None,
                        status: None,
                        page_size: None,
                        page_token: None,
                        history_length: None,
                        status_timestamp_after: None,
                        include_artifacts: None,
                        tenant: None,
                    },
                )
                .await,
        );
        assert_unsupported_operation(
            transport
                .cancel_task(
                    &params,
                    &CancelTaskRequest {
                        id: "task-1".to_string(),
                        metadata: None,
                        tenant: None,
                    },
                )
                .await,
        );
        assert_unsupported_operation(
            transport
                .subscribe_to_task(
                    &params,
                    &SubscribeToTaskRequest {
                        id: "task-1".to_string(),
                        tenant: None,
                    },
                )
                .await,
        );
        assert_unsupported_operation(
            transport
                .create_push_config(
                    &params,
                    &TaskPushNotificationConfig {
                        task_id: "task-1".to_string(),
                        url: "https://example.com/callback".to_string(),
                        id: Some("cfg-1".to_string()),
                        token: None,
                        authentication: None,
                        tenant: None,
                    },
                )
                .await,
        );
        assert_unsupported_operation(
            transport
                .get_push_config(
                    &params,
                    &GetTaskPushNotificationConfigRequest {
                        task_id: "task-1".to_string(),
                        id: "cfg-1".to_string(),
                        tenant: None,
                    },
                )
                .await,
        );
        assert_unsupported_operation(
            transport
                .list_push_configs(
                    &params,
                    &ListTaskPushNotificationConfigsRequest {
                        task_id: "task-1".to_string(),
                        page_size: None,
                        page_token: None,
                        tenant: None,
                    },
                )
                .await,
        );
        assert_unsupported_operation(
            transport
                .delete_push_config(
                    &params,
                    &DeleteTaskPushNotificationConfigRequest {
                        task_id: "task-1".to_string(),
                        id: "cfg-1".to_string(),
                        tenant: None,
                    },
                )
                .await,
        );
        assert_unsupported_operation(
            transport
                .get_extended_agent_card(&params, &GetExtendedAgentCardRequest { tenant: None })
                .await,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_executes_all_commands_in_lib_tests() {
        let server = RunTestServer::spawn().await;

        run_test_cli(&server.base_url, &["card", "get"])
            .await
            .unwrap();
        run_test_cli(
            &server.base_url,
            &["--compact", "card", "get", "--extended"],
        )
        .await
        .unwrap();
        run_test_cli(
            &server.base_url,
            &[
                "--transport",
                "jsonrpc",
                "--bearer",
                "secret",
                "--svc-param",
                "X-Test: 123",
                "send",
                "hello from unit test",
                "--task-id",
                "task-send",
                "--context-id",
                "ctx-send",
            ],
        )
        .await
        .unwrap();
        run_test_cli(
            &server.base_url,
            &[
                "--compact",
                "send",
                "streaming request",
                "--stream",
                "--task-id",
                "task-stream",
                "--context-id",
                "ctx-stream",
            ],
        )
        .await
        .unwrap();
        run_test_cli(&server.base_url, &["task", "get", "task-send"])
            .await
            .unwrap();
        run_test_cli(
            &server.base_url,
            &[
                "--compact",
                "task",
                "list",
                "--context-id",
                "ctx-send",
                "--status",
                "completed",
            ],
        )
        .await
        .unwrap();
        run_test_cli(&server.base_url, &["task", "cancel", "task-send"])
            .await
            .unwrap();
        run_test_cli(
            &server.base_url,
            &["--compact", "task", "subscribe", "task-stream"],
        )
        .await
        .unwrap();
        run_test_cli(
            &server.base_url,
            &[
                "task",
                "push-config",
                "create",
                "task-1",
                "https://example.com/callback",
                "--config-id",
                "cfg-1",
            ],
        )
        .await
        .unwrap();
        run_test_cli(
            &server.base_url,
            &["--compact", "task", "push-config", "get", "task-1", "cfg-1"],
        )
        .await
        .unwrap();
        run_test_cli(
            &server.base_url,
            &["--compact", "task", "push-config", "list", "task-1"],
        )
        .await
        .unwrap();
        run_test_cli(
            &server.base_url,
            &["task", "push-config", "delete", "task-1", "cfg-1"],
        )
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_surfaces_errors_in_lib_tests() {
        let server = RunTestServer::spawn().await;

        let err = run_test_cli(
            &server.base_url,
            &["card", "get", "--extended", "--tenant", "error"],
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            CliError::A2A(error) if error.code == a2a::error_code::UNSUPPORTED_OPERATION
        ));

        let err = run_test_cli(&server.base_url, &["send", "send-error"])
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CliError::A2A(error) if error.code == a2a::error_code::INVALID_REQUEST
        ));

        let err = run_test_cli(&server.base_url, &["task", "list", "--context-id", "error"])
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CliError::A2A(error) if error.code == a2a::error_code::INVALID_PARAMS
        ));

        let err = run_test_cli(
            &server.base_url,
            &["--compact", "send", "stream-error", "--stream"],
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            CliError::A2A(error) if error.code == a2a::error_code::INTERNAL_ERROR
        ));

        let err = run_test_cli(
            &server.base_url,
            &["--compact", "task", "subscribe", "stream-error"],
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            CliError::A2A(error) if error.code == a2a::error_code::INTERNAL_ERROR
        ));

        let err = run_test_cli(
            &server.base_url,
            &[
                "task",
                "push-config",
                "create",
                "missing",
                "https://example.com/callback",
                "--config-id",
                "cfg-missing",
            ],
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            CliError::A2A(error) if error.code == a2a::error_code::TASK_NOT_FOUND
        ));

        let err = run_test_cli(
            &server.base_url,
            &["task", "push-config", "list", "missing"],
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            CliError::A2A(error) if error.code == a2a::error_code::TASK_NOT_FOUND
        ));

        let base_url = unused_base_url().await;
        let err = run_test_cli(&base_url, &["card", "get"]).await.unwrap_err();
        assert!(matches!(err, CliError::Http(_)));
    }

    #[test]
    fn test_task_state_conversion() {
        let cases = [
            (TaskStateArg::Unspecified, TaskState::Unspecified),
            (TaskStateArg::Submitted, TaskState::Submitted),
            (TaskStateArg::Working, TaskState::Working),
            (TaskStateArg::Completed, TaskState::Completed),
            (TaskStateArg::Failed, TaskState::Failed),
            (TaskStateArg::Canceled, TaskState::Canceled),
            (TaskStateArg::InputRequired, TaskState::InputRequired),
            (TaskStateArg::Rejected, TaskState::Rejected),
            (TaskStateArg::AuthRequired, TaskState::AuthRequired),
        ];

        for (input, expected) in cases {
            assert_eq!(TaskState::from(input), expected);
        }
    }

    #[test]
    fn test_means_not_a_readable_path_classification() {
        use std::io::ErrorKind;

        // "Doesn't name a readable file here" — --data-part goes on to try
        // the value as inline JSON. InvalidFilename is the Windows answer
        // for a path containing `{`, `"` or `:`, i.e. inline JSON itself,
        // and is why this can't just test for NotFound.
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::InvalidFilename,
            ErrorKind::InvalidInput,
        ] {
            assert!(
                means_not_a_readable_path(kind),
                "{kind:?} should fall through to inline-JSON parsing"
            );
        }

        // A real file that failed to read — must be reported, not masked.
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::IsADirectory,
            ErrorKind::InvalidData,
        ] {
            assert!(
                !means_not_a_readable_path(kind),
                "{kind:?} should be surfaced as a read error"
            );
        }
    }

    #[test]
    fn test_parse_duration_variants() {
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
    }

    #[test]
    fn test_parse_duration_rejects_invalid_input() {
        assert!(parse_duration("banana").is_err());
        assert!(parse_duration("-1s").is_err());
    }

    #[test]
    fn test_is_settled_classifies_task_states() {
        let settled = [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
            TaskState::InputRequired,
            TaskState::AuthRequired,
        ];
        for state in settled {
            assert!(is_settled(&state), "{state:?} should be settled");
        }

        let unsettled = [
            TaskState::Unspecified,
            TaskState::Submitted,
            TaskState::Working,
        ];
        for state in unsettled {
            assert!(!is_settled(&state), "{state:?} should not be settled");
        }
    }

    fn parse_with_matches(args: &[&str]) -> (Cli, ArgMatches) {
        let mut argv = vec!["a2acli".to_string()];
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        let matches = Cli::command().try_get_matches_from(argv).unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        (cli, matches)
    }

    fn send_command(cli: &Cli) -> &MessageCommand {
        match &cli.command {
            Command::Send(command) => command,
            other => panic!("expected Command::Send, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_message_parts_preserves_interleaved_order_and_media_type() {
        let (cli, matches) = parse_with_matches(&[
            "send",
            "--text-part",
            "hello",
            "--file-part",
            "https://example.com/doc.pdf",
            "--media-type",
            "application/pdf",
            "--data-part",
            r#"{"k":1}"#,
        ]);
        let send_matches = matches.subcommand_matches("send").unwrap();
        let parts = resolve_message_parts(send_matches, send_command(&cli)).unwrap();

        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].as_text(), Some("hello"));
        assert!(parts[0].media_type.is_none());
        assert!(matches!(
            &parts[1].content,
            PartContent::Url(url) if url == "https://example.com/doc.pdf"
        ));
        assert_eq!(parts[1].media_type.as_deref(), Some("application/pdf"));
        assert!(matches!(&parts[2].content, PartContent::Data(_)));
        assert!(parts[2].media_type.is_none());
    }

    #[test]
    fn test_resolve_message_parts_positional_text_is_shorthand() {
        let (cli, matches) = parse_with_matches(&["send", "hello"]);
        let send_matches = matches.subcommand_matches("send").unwrap();
        let parts = resolve_message_parts(send_matches, send_command(&cli)).unwrap();

        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].as_text(), Some("hello"));
    }

    #[test]
    fn test_resolve_message_parts_rejects_media_type_without_preceding_part() {
        let (cli, matches) =
            parse_with_matches(&["send", "--media-type", "text/plain", "--text-part", "hi"]);
        let send_matches = matches.subcommand_matches("send").unwrap();
        let err = resolve_message_parts(send_matches, send_command(&cli)).unwrap_err();
        assert!(matches!(err, CliError::InvalidInput(_)));
    }

    #[test]
    fn test_resolve_message_parts_rejects_positional_text_with_part_flags() {
        let (cli, matches) = parse_with_matches(&["send", "hello", "--text-part", "world"]);
        let send_matches = matches.subcommand_matches("send").unwrap();
        let err = resolve_message_parts(send_matches, send_command(&cli)).unwrap_err();
        assert!(matches!(err, CliError::InvalidInput(_)));
    }

    #[test]
    fn test_cli_error_exit_codes_and_envelope_codes() {
        let a2a = CliError::A2A(A2AError::task_not_found("t-1"));
        assert_eq!(a2a.exit_code(), 1);
        let envelope = a2a.envelope();
        assert_eq!(envelope.error.code, "TASK_NOT_FOUND");
        assert_eq!(envelope.error.a2a_code, Some(-32001));

        let usage = CliError::InvalidInput("bad flag".to_string());
        assert_eq!(usage.exit_code(), 2);
        assert_eq!(usage.envelope().error.code, "A2ACLI_ERR_USAGE");
        assert!(usage.envelope().error.a2a_code.is_none());

        let read_file = CliError::ReadFile {
            path: "missing.bin".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        assert_eq!(read_file.exit_code(), 2);
        assert_eq!(read_file.envelope().error.code, "A2ACLI_ERR_USAGE");

        let timeout = CliError::Timeout {
            task_id: "t-2".to_string(),
            timeout: Duration::from_secs(30),
        };
        assert_eq!(timeout.exit_code(), 5);
        assert_eq!(timeout.envelope().error.code, "A2ACLI_ERR_TIMEOUT");

        let json = CliError::Json(serde_json::from_str::<serde_json::Value>("{").unwrap_err());
        assert_eq!(json.exit_code(), 1);
        assert_eq!(json.envelope().error.code, "A2ACLI_ERR_INTERNAL");
    }

    #[test]
    fn test_cli_error_report_prints_one_json_line_to_stderr() {
        // report() itself only writes to the real process stderr, which
        // isn't capturable here; assert on the envelope it serializes
        // instead, since that's the part the contract cares about.
        let error = CliError::A2A(A2AError::unsupported_operation("nope"));
        let json = serde_json::to_string(&error.envelope()).unwrap();
        assert!(json.contains("\"code\":\"UNSUPPORTED_OPERATION\""));
        assert!(!json.contains('\n'));
    }

    #[test]
    fn test_agent_card_render_text() {
        let card = AgentCard {
            name: "Fixture".to_string(),
            description: "desc".to_string(),
            version: VERSION.to_string(),
            supported_interfaces: vec![AgentInterface::new(
                "http://host/jsonrpc",
                TRANSPORT_PROTOCOL_JSONRPC,
            )],
            capabilities: AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                extensions: None,
                extended_agent_card: None,
            },
            default_input_modes: vec![],
            default_output_modes: vec![],
            skills: vec![],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        };

        let text = card.render_text();
        assert!(text.contains("Name: Fixture"));
        assert!(text.contains("Streaming: true"));
        assert!(text.contains("Push Notifications: false"));
        assert!(text.contains("Extended Card: false"));
        assert!(text.contains("Interfaces:"));
        assert!(text.contains("http://host/jsonrpc"));
        assert!(text.contains("Skills: 0"));
    }

    #[test]
    fn test_message_render_text_renders_every_part_kind() {
        let mut file_part = Part::raw(vec![1, 2, 3]);
        file_part.filename = Some("report.bin".to_string());
        file_part.media_type = Some("application/octet-stream".to_string());

        let message = Message::new(
            Role::Agent,
            vec![
                Part::text("hello"),
                Part::data(serde_json::json!({"k": 1})),
                file_part,
            ],
        );

        let text = message.render_text();
        assert!(text.contains("Text:\nhello"));
        assert!(text.contains("Data:"));
        assert!(text.contains("\"k\": 1"));
        assert!(text.contains("File: report.bin (application/octet-stream, 3 bytes)"));
    }

    /// §10.3: a part whose media type the agent didn't declare still has to
    /// render — by name and size for inline bytes, by URL for a referenced
    /// file — rather than being dropped because the media type is missing.
    #[test]
    fn test_render_part_without_media_type_falls_back_to_name_and_url() {
        let message = Message::new(
            Role::Agent,
            vec![
                Part::raw(vec![7; 12]),
                Part::raw(vec![1, 2]).with_filename("notes.txt"),
                Part::url("https://example.com/spec.pdf"),
                Part::url("https://example.com/report.csv").with_media_type("text/csv"),
            ],
        );

        let text = message.render_text();
        // No filename and no media type: neither is invented, and the part
        // is still accounted for.
        assert!(text.contains("File: (unnamed) (12 bytes)"), "{text}");
        assert!(text.contains("File: notes.txt (2 bytes)"), "{text}");
        assert!(
            text.contains("File: https://example.com/spec.pdf"),
            "{text}"
        );
        assert!(
            text.contains("File: https://example.com/report.csv (text/csv)"),
            "{text}"
        );
    }

    /// A message with no parts renders an explicit `(none)` rather than
    /// nothing at all, so `text` output never leaves the reader unsure
    /// whether content was omitted or absent.
    #[test]
    fn test_message_render_text_marks_an_empty_part_list() {
        let message = Message {
            message_id: "msg-1".to_string(),
            context_id: Some("ctx-1".to_string()),
            task_id: Some("task-1".to_string()),
            role: Role::Agent,
            parts: vec![],
            metadata: None,
            extensions: None,
            reference_task_ids: None,
        };

        let text = message.render_text();
        assert!(text.contains("Context ID: ctx-1"), "{text}");
        assert!(text.contains("Task ID: task-1"), "{text}");
        assert!(text.contains("Parts: (none)"), "{text}");
    }

    /// A `send` that answers with a bare `Message` instead of a `Task`
    /// renders through the message form (§10.2) — the response enum must not
    /// have a task-shaped rendering as its only arm.
    #[test]
    fn test_send_message_response_renders_the_message_arm() {
        let message = Message::new(Role::Agent, vec![Part::text("no task needed")]);
        let response = SendMessageResponse::Message(message.clone());

        assert_eq!(response.render_text(), message.render_text());
        assert!(response.render_text().contains("no task needed"));
    }

    /// A card that declares no compatible interface renders `(none)` under
    /// `Interfaces:` — `card get` is exactly the command you reach for to
    /// find out *why* a transport couldn't be selected, so an empty list is
    /// the case it most needs to state plainly.
    #[test]
    fn test_agent_card_render_text_marks_an_empty_interface_list() {
        let card = AgentCard {
            name: "Bare".to_string(),
            description: "no interfaces".to_string(),
            version: VERSION.to_string(),
            supported_interfaces: vec![],
            capabilities: AgentCapabilities {
                streaming: None,
                push_notifications: None,
                extensions: None,
                extended_agent_card: None,
            },
            default_input_modes: vec![],
            default_output_modes: vec![],
            skills: vec![],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        };

        let text = card.render_text();
        assert!(text.contains("Interfaces:\n  (none)"), "{text}");
        // Absent capabilities read as `false`, never as a missing line.
        assert!(text.contains("Streaming: false"), "{text}");
    }

    /// §10.3: artifacts are rendered per artifact, labeled by name when the
    /// agent gave one and by artifact id when it didn't, with their parts
    /// rendered underneath — never summarized as a count.
    #[test]
    fn test_task_render_text_lists_artifacts_and_resume_hint() {
        let mut task = make_fixture_task(
            "task-art",
            "ctx-art",
            TaskState::InputRequired,
            "which region?",
        );
        task.artifacts = Some(vec![
            Artifact {
                artifact_id: "art-1".to_string(),
                name: Some("summary".to_string()),
                description: None,
                parts: vec![Part::text("all clear")],
                metadata: None,
                extensions: None,
            },
            Artifact {
                artifact_id: "art-2".to_string(),
                name: None,
                description: None,
                parts: vec![Part::data(serde_json::json!({"rows": 3}))],
                metadata: None,
                extensions: None,
            },
        ]);

        let text = task.render_text();
        assert!(text.contains("Artifact: summary"), "{text}");
        assert!(text.contains("all clear"), "{text}");
        // Unnamed artifact falls back to its id rather than rendering blank.
        assert!(text.contains("Artifact: art-2"), "{text}");
        assert!(text.contains("\"rows\": 3"), "{text}");
        // §9.2: an interrupted task always carries the resume command.
        assert!(
            text.contains("Resume with: a2acli send --task-id task-art \"<reply>\""),
            "{text}"
        );
    }

    /// §9.1: every task state has a distinct short label, and none of them
    /// renders as the protocol's wire spelling — that form belongs to
    /// `-o json` only.
    #[test]
    fn test_task_state_label_covers_every_state() {
        let cases = [
            (TaskState::Unspecified, "UNSPECIFIED"),
            (TaskState::Submitted, "SUBMITTED"),
            (TaskState::Working, "WORKING"),
            (TaskState::Completed, "COMPLETED"),
            (TaskState::Failed, "FAILED"),
            (TaskState::Canceled, "CANCELED"),
            (TaskState::InputRequired, "INPUT_REQUIRED"),
            (TaskState::Rejected, "REJECTED"),
            (TaskState::AuthRequired, "AUTH_REQUIRED"),
        ];

        for (state, expected) in cases {
            assert_eq!(task_state_label(&state), expected);
            assert!(!expected.starts_with("TASK_STATE_"));
        }
    }

    #[test]
    fn test_list_tasks_render_text_empty_and_paged() {
        let empty = ListTasksResponse {
            tasks: vec![],
            next_page_token: "next-token".to_string(),
            page_size: 10,
            total_size: 0,
        };

        let text = empty.render_text();
        assert!(text.contains("Total: 0"), "{text}");
        assert!(text.contains("Page Size: 10"), "{text}");
        assert!(text.contains("Next Page Token: next-token"), "{text}");
        assert!(text.contains("Tasks: (none)"), "{text}");

        let populated = ListTasksResponse {
            tasks: vec![make_fixture_task(
                "task-1",
                "ctx-1",
                TaskState::Completed,
                "done",
            )],
            next_page_token: String::new(),
            page_size: 10,
            total_size: 1,
        };

        let text = populated.render_text();
        // An empty page token is omitted rather than rendered as a blank
        // field, so `text` output never suggests there is a next page.
        assert!(!text.contains("Next Page Token"), "{text}");
        assert!(text.contains("Task:"), "{text}");
        assert!(text.contains("Task ID: task-1"), "{text}");
    }

    #[test]
    fn test_push_config_render_text_includes_token_and_auth_scheme() {
        let config = TaskPushNotificationConfig {
            url: "https://example.com/hook".to_string(),
            id: Some("cfg-1".to_string()),
            task_id: "task-1".to_string(),
            token: Some("tok-1".to_string()),
            authentication: Some(AuthenticationInfo {
                scheme: "Bearer".to_string(),
                credentials: Some("super-secret".to_string()),
            }),
            tenant: None,
        };

        let text = config.render_text();
        assert!(text.contains("Config ID: cfg-1"), "{text}");
        assert!(text.contains("Task ID: task-1"), "{text}");
        assert!(text.contains("URL: https://example.com/hook"), "{text}");
        assert!(text.contains("Token: tok-1"), "{text}");
        assert!(text.contains("Auth Scheme: Bearer"), "{text}");
        // The scheme is useful; the credential behind it is never echoed.
        assert!(!text.contains("super-secret"), "{text}");
    }

    #[test]
    fn test_list_push_configs_render_text_empty_and_paged() {
        let empty = ListTaskPushNotificationConfigsResponse {
            configs: vec![],
            next_page_token: Some("next-token".to_string()),
        };

        let text = empty.render_text();
        assert!(text.contains("Next Page Token: next-token"), "{text}");
        assert!(text.contains("Configs: (none)"), "{text}");

        let populated = ListTaskPushNotificationConfigsResponse {
            configs: vec![TaskPushNotificationConfig {
                url: "https://example.com/hook".to_string(),
                id: Some("cfg-1".to_string()),
                task_id: "task-1".to_string(),
                token: None,
                authentication: None,
                tenant: None,
            }],
            next_page_token: None,
        };

        let text = populated.render_text();
        assert!(!text.contains("Next Page Token"), "{text}");
        assert!(text.contains("Push Config:"), "{text}");
        assert!(text.contains("Config ID: cfg-1"), "{text}");
    }

    #[test]
    fn test_push_config_deleted_render_text() {
        let deleted = PushConfigDeleted {
            deleted: true,
            task_id: "task-1".to_string(),
            id: "cfg-1".to_string(),
        };

        let text = deleted.render_text();
        assert!(text.contains("Deleted: true"), "{text}");
        assert!(text.contains("Task ID: task-1"), "{text}");
        assert!(text.contains("Config ID: cfg-1"), "{text}");
    }

    /// Under `--stream` each event is rendered on its own, so the update
    /// events need renderings of their own — a status update that pauses the
    /// task still has to carry the resume command (§9.2), and an artifact
    /// update still has to show the artifact's parts (§10.3).
    #[test]
    fn test_stream_response_render_text_covers_update_events() {
        let status = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "task-s".to_string(),
            context_id: "ctx-s".to_string(),
            status: TaskStatus {
                state: TaskState::AuthRequired,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });

        let text = status.render_text();
        assert!(text.contains("Task ID: task-s"), "{text}");
        assert!(text.contains("Context ID: ctx-s"), "{text}");
        assert!(text.contains("State: AUTH_REQUIRED"), "{text}");
        assert!(
            text.contains("Resume with: a2acli send --task-id task-s \"<reply>\""),
            "{text}"
        );

        // A state that isn't interrupted gets no resume line to act on.
        let working = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "task-s".to_string(),
            context_id: "ctx-s".to_string(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        assert!(!working.render_text().contains("Resume with"));

        let artifact = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: "task-s".to_string(),
            context_id: "ctx-s".to_string(),
            artifact: Artifact {
                artifact_id: "art-9".to_string(),
                name: None,
                description: None,
                parts: vec![Part::text("chunk one")],
                metadata: None,
                extensions: None,
            },
            append: None,
            last_chunk: None,
            metadata: None,
        });

        let text = artifact.render_text();
        assert!(text.contains("Task ID: task-s"), "{text}");
        assert!(text.contains("Artifact: art-9"), "{text}");
        assert!(text.contains("chunk one"), "{text}");

        // A stream may also carry whole tasks and messages, which render
        // through their own forms rather than a stream-specific one.
        let task = make_fixture_task("task-s", "ctx-s", TaskState::Completed, "done");
        assert_eq!(
            StreamResponse::Task(task.clone()).render_text(),
            task.render_text()
        );
        let message = Message::new(Role::Agent, vec![Part::text("interim")]);
        assert_eq!(
            StreamResponse::Message(message.clone()).render_text(),
            message.render_text()
        );
    }

    fn text_cli() -> Cli {
        parse_with_matches(&["task", "get", "unused"]).0
    }

    /// `text` mode renders through [`TextRender`] and never through
    /// `Serialize`, so a value that cannot be serialized still streams — the
    /// mirror of `test_consume_stream_reports_json_error`.
    #[tokio::test]
    async fn test_consume_stream_text_mode_does_not_serialize() {
        let stream = Box::pin(stream::once(async { Ok(FailingSerialize) }));
        consume_stream(make_test_client(None), stream, &text_cli())
            .await
            .unwrap();
    }

    /// Malformed JSON in a `--data-part` file is the caller's input to fix,
    /// so it surfaces as a usage error naming the source — not as the
    /// generic internal `CliError::Json`.
    #[test]
    fn test_parse_data_part_json_rejects_malformed_content() {
        let error = parse_data_part_json("{not json", "payload.json").unwrap_err();

        assert_eq!(error.exit_code(), 2);
        let message = error.to_string();
        assert!(message.contains("payload.json"), "{message}");
        assert!(matches!(error, CliError::InvalidInput(_)));
    }

    /// A server that answers the well-known Agent Card path with exactly one
    /// canned response and serves nothing else — enough to drive Appendix
    /// D's classification of a card fetch, which needs a real
    /// `reqwest::Error` (the type has no public constructor, so the
    /// classification can't be asserted on a synthetic one).
    struct CardOnlyServer {
        base_url: String,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for CardOnlyServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    impl CardOnlyServer {
        async fn spawn(status: axum::http::StatusCode, body: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new().route(
                WELL_KNOWN_AGENT_CARD_PATH,
                get(move || async move {
                    (
                        status,
                        [(header::CONTENT_TYPE, "application/json")],
                        body.to_string(),
                    )
                }),
            );
            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            CardOnlyServer { base_url, handle }
        }
    }

    /// Appendix D: a card fetch distinguishes "the agent isn't there" from
    /// "the agent refused you" from "that isn't an agent card", because the
    /// three need different fixes — and each maps to its own exit status
    /// (§11.6) so a script can branch on it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_card_fetch_failures_are_classified_per_appendix_d() {
        let not_found = CardOnlyServer::spawn(axum::http::StatusCode::NOT_FOUND, "{}").await;
        let error = run_test_cli(&not_found.base_url, &["card", "get"])
            .await
            .unwrap_err();
        assert_eq!(error.envelope().error.code, "A2ACLI_ERR_CARD_NOT_FOUND");
        assert_eq!(error.exit_code(), 3);
        assert!(error.envelope().error.hint.is_some());

        for status in [
            axum::http::StatusCode::UNAUTHORIZED,
            axum::http::StatusCode::FORBIDDEN,
        ] {
            let denied = CardOnlyServer::spawn(status, "{}").await;
            let error = run_test_cli(&denied.base_url, &["card", "get"])
                .await
                .unwrap_err();
            assert_eq!(
                error.envelope().error.code,
                "A2ACLI_ERR_AUTH_FAILED",
                "status {status}"
            );
            assert_eq!(error.exit_code(), 4, "status {status}");
        }

        // 200, but the body isn't an Agent Card: the agent answered, so this
        // is neither unreachable nor a missing card.
        let invalid =
            CardOnlyServer::spawn(axum::http::StatusCode::OK, r#"{"not":"a card"}"#).await;
        let error = run_test_cli(&invalid.base_url, &["card", "get"])
            .await
            .unwrap_err();
        assert_eq!(error.envelope().error.code, "A2ACLI_ERR_CARD_INVALID");
        assert_eq!(error.exit_code(), 1);

        // An unreachable agent stays distinct from all of the above.
        let base_url = unused_base_url().await;
        let error = run_test_cli(&base_url, &["card", "get"]).await.unwrap_err();
        assert_eq!(error.envelope().error.code, "A2ACLI_ERR_UNREACHABLE");
        assert_eq!(error.exit_code(), 3);
    }

    #[test]
    fn test_parse_dotenv_basic() {
        let parsed = parse_dotenv("A2ACLI_BEARER=secret\nA2ACLI_TRANSPORT=jsonrpc,rest\n");
        assert_eq!(
            parsed,
            vec![
                ("A2ACLI_BEARER".to_string(), "secret".to_string()),
                ("A2ACLI_TRANSPORT".to_string(), "jsonrpc,rest".to_string()),
            ]
        );
    }

    #[test]
    fn test_parse_dotenv_ignores_blank_lines_and_comments() {
        let parsed = parse_dotenv("\n# a comment\n   \nA2ACLI_TENANT=acme\n# another comment\n");
        assert_eq!(
            parsed,
            vec![("A2ACLI_TENANT".to_string(), "acme".to_string())]
        );
    }

    #[test]
    fn test_parse_dotenv_tolerates_export_prefix() {
        let parsed = parse_dotenv("export A2ACLI_BASE_URL=https://agent.example.com\n");
        assert_eq!(
            parsed,
            vec![(
                "A2ACLI_BASE_URL".to_string(),
                "https://agent.example.com".to_string()
            )]
        );
    }

    #[test]
    fn test_parse_dotenv_strips_one_layer_of_quotes() {
        let parsed = parse_dotenv("A2ACLI_BEARER=\"Bearer token\"\nA2ACLI_TENANT='acme'\n");
        assert_eq!(
            parsed,
            vec![
                ("A2ACLI_BEARER".to_string(), "Bearer token".to_string()),
                ("A2ACLI_TENANT".to_string(), "acme".to_string()),
            ]
        );
    }

    #[test]
    fn test_find_flag_value_supports_space_and_equals_forms() {
        let space = vec![
            OsString::from("a2acli"),
            OsString::from("--config"),
            OsString::from("prod.env"),
        ];
        assert_eq!(
            find_flag_value(&space, "--config"),
            Some("prod.env".to_string())
        );

        let equals = vec![
            OsString::from("a2acli"),
            OsString::from("--config=prod.env"),
        ];
        assert_eq!(
            find_flag_value(&equals, "--config"),
            Some("prod.env".to_string())
        );

        let absent = vec![OsString::from("a2acli"), OsString::from("send")];
        assert_eq!(find_flag_value(&absent, "--config"), None);
    }

    #[test]
    fn test_config_source_labels() {
        assert_eq!(ConfigSource::Flag.label(), "flag");
        assert_eq!(ConfigSource::EnvVar.label(), "environment variable");
        assert_eq!(ConfigSource::LocalFile.label(), "local .env file");
        assert_eq!(ConfigSource::GlobalFile.label(), "global .env file");
        assert_eq!(ConfigSource::Default.label(), "built-in default");
    }

    #[test]
    fn test_redact_secret() {
        assert_eq!(
            redact_secret(&Some("secret".to_string())),
            "(set, redacted)"
        );
        assert_eq!(redact_secret(&None), "(not set)");
    }

    /// §10.1's three reference forms, and the scheme a bare host gets. These
    /// mirror the reference implementation's `normalize` in
    /// `internal/flagparse/urlorpath.go`: loopback gets `http://` because a
    /// local development agent is rarely served over TLS, everything else
    /// gets `https://`, and a reference that already carries a path is a
    /// full card URL rather than an origin.
    #[test]
    fn test_normalize_card_reference_host_forms() {
        let cases = [
            // Bare origin -> well-known path appended.
            (
                "example.com",
                "https://example.com/.well-known/agent-card.json",
            ),
            (
                "http://example.com",
                "http://example.com/.well-known/agent-card.json",
            ),
            (
                "https://example.com/",
                "https://example.com/.well-known/agent-card.json",
            ),
            // Loopback -> http, since a dev agent is rarely behind TLS.
            (
                "localhost:3000",
                "http://localhost:3000/.well-known/agent-card.json",
            ),
            (
                "127.0.0.1:8080",
                "http://127.0.0.1:8080/.well-known/agent-card.json",
            ),
            (
                "[::1]:9000",
                "http://[::1]:9000/.well-known/agent-card.json",
            ),
            // Already carries a path -> a full card URL, used as-is.
            (
                "https://example.com/custom/card.json",
                "https://example.com/custom/card.json",
            ),
            (
                "http://example.com/agents/a/card",
                "http://example.com/agents/a/card",
            ),
        ];

        for (reference, expected) in cases {
            assert_eq!(
                normalize_card_reference(reference),
                CardReference::Url(expected.to_string()),
                "reference {reference}"
            );
        }
    }

    #[test]
    fn test_normalize_card_reference_file_forms() {
        // `file://` is explicit and needs no filesystem check.
        assert_eq!(
            normalize_card_reference("file:///tmp/card.json"),
            CardReference::File(PathBuf::from("/tmp/card.json"))
        );
        // A path-shaped reference is a path even when nothing is there, so a
        // typo reports a missing file rather than being fetched over HTTPS.
        for reference in ["/no/such/card.json", "./card.json", "../card.json"] {
            assert_eq!(
                normalize_card_reference(reference),
                CardReference::File(PathBuf::from(reference)),
                "reference {reference}"
            );
        }
    }

    /// Windows path shapes, asserted as plain strings so they are checked on
    /// every platform. macOS and Linux cannot reproduce the divergence — a
    /// Unix path starts with `/` and is caught by the first prefix — so only
    /// the Windows CI job would otherwise notice a regression here, which is
    /// exactly how this was found (a2aproject/a2a-rs#190).
    #[test]
    fn test_windows_path_shapes_are_recognized_as_files() {
        for reference in [
            r"C:\Users\me\card.json",
            r"c:/Users/me/card.json",
            r"\\server\share\card.json",
            r".\card.json",
            r"..\card.json",
        ] {
            assert!(looks_like_file_path(reference), "{reference}");
            assert_eq!(
                normalize_card_reference(reference),
                CardReference::File(PathBuf::from(reference)),
                "{reference}"
            );
        }

        // A `host:port` is not a drive root, however short the host: the
        // byte after the colon is a digit, not a path separator.
        for reference in ["localhost:3000", "a:3000", "example.com:443"] {
            assert!(!has_windows_drive_prefix(reference), "{reference}");
            assert!(!looks_like_file_path(reference), "{reference}");
        }

        // `file:///C:/…`: the slash before the drive letter is URL syntax,
        // not part of the path.
        assert_eq!(
            normalize_card_reference("file:///C:/Users/me/card.json"),
            CardReference::File(PathBuf::from("C:/Users/me/card.json"))
        );
        // On Unix the same leading slash *is* the root, and is kept.
        assert_eq!(
            normalize_card_reference("file:///tmp/card.json"),
            CardReference::File(PathBuf::from("/tmp/card.json"))
        );
    }

    /// A bare name that happens to exist on disk is treated as a file, the
    /// same way the reference implementation stats the reference before
    /// falling back to the host forms.
    #[test]
    fn test_normalize_card_reference_prefers_an_existing_file_over_a_host() {
        let dir = std::env::temp_dir().join(format!("a2acli-norm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("example.com");
        std::fs::write(&path, "{}").unwrap();

        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let resolved = normalize_card_reference("example.com");
        std::env::set_current_dir(previous).unwrap();

        assert_eq!(resolved, CardReference::File(PathBuf::from("example.com")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_loopback_host() {
        for host in [
            "localhost",
            "localhost:3000",
            "127.0.0.1",
            "127.0.0.1:80",
            "::1",
            "[::1]:9000",
        ] {
            assert!(is_loopback_host(host), "expected loopback: {host}");
        }
        for host in [
            "example.com",
            "example.com:443",
            "8.8.8.8",
            "2606:4700::1111",
        ] {
            assert!(!is_loopback_host(host), "expected non-loopback: {host}");
        }
    }

    #[test]
    fn test_card_file_errors_map_to_appendix_d_codes() {
        let missing = CliError::CardFile {
            path: "/no/such/card.json".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        assert_eq!(missing.envelope().error.code, "A2ACLI_ERR_CARD_NOT_FOUND");
        assert_eq!(missing.exit_code(), 3);

        let invalid = CliError::CardInvalid("card.json: missing field `name`".to_string());
        assert_eq!(invalid.envelope().error.code, "A2ACLI_ERR_CARD_INVALID");
        assert_eq!(invalid.exit_code(), 1);
    }

    /// The §7.2 selection matrix, in one place: the two flags are exclusive,
    /// `--endpoint` needs exactly one transport, and absent both the
    /// deprecated `--base-url` still supplies the reference.
    #[test]
    fn test_resolve_agent_selection_matrix() {
        let (cli, matches) = parse_with_matches(&["--agent-card", "example.com", "card", "get"]);
        assert_eq!(
            resolve_agent_selection(&cli, &matches).unwrap(),
            AgentSelection::Card("example.com".to_string())
        );

        // Neither flag: falls back to --base-url at its built-in default,
        // and does not warn, because nothing deprecated was passed.
        let (cli, matches) = parse_with_matches(&["card", "get"]);
        assert_eq!(
            resolve_agent_selection(&cli, &matches).unwrap(),
            AgentSelection::Card("http://localhost:3000".to_string())
        );

        // --base-url explicitly: still honored (the alias keeps working).
        let (cli, matches) = parse_with_matches(&["--base-url", "http://host:1", "card", "get"]);
        assert_eq!(
            resolve_agent_selection(&cli, &matches).unwrap(),
            AgentSelection::Card("http://host:1".to_string())
        );

        let (cli, matches) = parse_with_matches(&[
            "--endpoint",
            "http://host/jsonrpc",
            "--transport",
            "jsonrpc",
            "card",
            "get",
        ]);
        assert_eq!(
            resolve_agent_selection(&cli, &matches).unwrap(),
            AgentSelection::Endpoint {
                url: "http://host/jsonrpc".to_string(),
                binding: Binding::Jsonrpc,
            }
        );

        // --endpoint with zero and with two transports: both usage errors.
        for transports in [vec![], vec!["jsonrpc", "rest"]] {
            let mut args = vec!["--endpoint", "http://host/jsonrpc"];
            for transport in &transports {
                args.push("--transport");
                args.push(transport);
            }
            args.extend(["card", "get"]);
            let (cli, matches) = parse_with_matches(&args);
            let error = resolve_agent_selection(&cli, &matches).unwrap_err();
            assert_eq!(
                error.exit_code(),
                2,
                "with {} transport(s)",
                transports.len()
            );
            assert!(error.to_string().contains("exactly one --transport"));
        }

        let (cli, matches) = parse_with_matches(&[
            "--agent-card",
            "example.com",
            "--endpoint",
            "http://host/jsonrpc",
            "--transport",
            "jsonrpc",
            "card",
            "get",
        ]);
        let error = resolve_agent_selection(&cli, &matches).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("mutually exclusive"));
    }

    /// `--endpoint` stands in a one-interface card so the rest of the client
    /// path is unchanged; it must carry exactly the named interface and
    /// declare no capabilities it cannot vouch for.
    #[test]
    fn test_synthesized_endpoint_card_carries_only_the_named_interface() {
        let card = synthesized_endpoint_card("http://host/rest", Binding::Rest);

        assert_eq!(card.supported_interfaces.len(), 1);
        assert_eq!(card.supported_interfaces[0].url, "http://host/rest");
        assert_eq!(
            card.supported_interfaces[0].protocol_binding,
            Binding::Rest.protocol()
        );
        assert!(card.description.contains("no agent card was resolved"));
        // No card was read, so nothing is advertised.
        assert_eq!(card.capabilities.streaming, None);
        assert!(card.skills.is_empty());
    }

    #[test]
    fn test_parse_protocol_version() {
        assert_eq!(parse_protocol_version("1"), Some((1, 0)));
        assert_eq!(parse_protocol_version("1.0"), Some((1, 0)));
        assert_eq!(parse_protocol_version("1.2"), Some((1, 2)));
        // A patch component is tolerated and ignored: A2A versions are
        // major.minor, and a card declaring 1.0.2 must still negotiate.
        assert_eq!(parse_protocol_version("1.0.2"), Some((1, 0)));
        assert_eq!(parse_protocol_version(" 1.1 "), Some((1, 1)));
        assert_eq!(parse_protocol_version("nonsense"), None);
        assert_eq!(parse_protocol_version(""), None);
        assert_eq!(parse_protocol_version("1.x"), None);
    }

    /// §13.2: anything outside 1.x is refused rather than downgraded,
    /// because A2A reads an empty or pre-1.0 version as 0.3.
    #[test]
    fn test_validate_a2a_version() {
        for accepted in [None, Some("1"), Some("1.0"), Some("1.7")] {
            assert!(
                validate_a2a_version(accepted).is_ok(),
                "should accept {accepted:?}"
            );
        }

        for rejected in ["0.3", "0.9", "2.0", "nonsense", ""] {
            let error = validate_a2a_version(Some(rejected)).unwrap_err();
            assert_eq!(error.exit_code(), 2, "rejecting {rejected}");
            assert_eq!(error.envelope().error.code, "A2ACLI_ERR_USAGE");
        }

        // The pre-1.0 refusal explains itself rather than just failing.
        let error = validate_a2a_version(Some("0.3")).unwrap_err();
        assert!(error.to_string().contains("must be 1.x"), "{error}");
    }

    #[test]
    fn test_negotiate_a2a_version_uses_an_explicit_flag_verbatim() {
        // Explicit and equal to what this build speaks: nothing to report.
        let negotiated = negotiate_a2a_version(Some("1.0"), "1.0", (1, 0));
        assert_eq!(negotiated.version, "1.0");
        assert!(negotiated.note.is_none());

        // Explicit and different: used as written, and said out loud, since
        // §13.2 forbids a silent change of the signaled version.
        let negotiated = negotiate_a2a_version(Some("1.3"), "1.0", (1, 0));
        assert_eq!(negotiated.version, "1.3");
        assert!(negotiated.note.unwrap().contains("1.3"));
    }

    /// Absent a flag, negotiate down to the highest version both sides
    /// declare — `supported` is a parameter precisely so this branch is
    /// exercisable while this build's own VERSION is still 1.0.
    #[test]
    fn test_negotiate_a2a_version_takes_the_highest_shared_1x() {
        // Agent declares less than we support: follow it down, and say so.
        let negotiated = negotiate_a2a_version(None, "1.1", (1, 3));
        assert_eq!(negotiated.version, "1.1");
        assert!(negotiated.note.unwrap().contains("negotiated"));

        // Agent declares more than we support: hold at ours, nothing to say.
        let negotiated = negotiate_a2a_version(None, "1.5", (1, 3));
        assert_eq!(negotiated.version, "1.3");
        assert!(negotiated.note.is_none());

        // Equal: no note.
        let negotiated = negotiate_a2a_version(None, "1.3", (1, 3));
        assert_eq!(negotiated.version, "1.3");
        assert!(negotiated.note.is_none());
    }

    /// The 1.0 floor: a card declaring a pre-1.0 or non-1.x version does not
    /// drag the tool into 0.3 semantics, and the refusal is reported.
    #[test]
    fn test_negotiate_a2a_version_never_goes_below_the_1x_floor() {
        for declared in ["0.3", "0.9", "2.0"] {
            let negotiated = negotiate_a2a_version(None, declared, (1, 0));
            assert_eq!(negotiated.version, "1.0", "declared {declared}");
            let note = negotiated.note.unwrap_or_default();
            assert!(note.contains("outside 1.x"), "declared {declared}: {note}");
        }

        // An unparseable declaration is also not a reason to downgrade.
        let negotiated = negotiate_a2a_version(None, "not-a-version", (1, 0));
        assert_eq!(negotiated.version, "1.0");
        assert!(negotiated.note.unwrap().contains("unparseable"));
    }

    /// EXIT_002 enumerates exactly four outcomes worth naming. `COMPLETED`
    /// is success and `CANCELED` is what `task cancel` was asked to produce,
    /// so neither says anything; the remaining states are mid-flight.
    #[test]
    fn test_task_outcome_note_names_only_non_success_and_paused_states() {
        for (state, expected) in [
            (TaskState::Failed, "FAILED"),
            (TaskState::Rejected, "REJECTED"),
            (TaskState::InputRequired, "INPUT_REQUIRED"),
            (TaskState::AuthRequired, "AUTH_REQUIRED"),
        ] {
            let note =
                task_outcome_note(&state).unwrap_or_else(|| panic!("{state:?} should be named"));
            assert!(note.contains(expected), "{state:?} -> {note}");
        }

        for state in [
            TaskState::Completed,
            TaskState::Canceled,
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Unspecified,
        ] {
            assert!(
                task_outcome_note(&state).is_none(),
                "{state:?} should stay silent"
            );
        }
    }

    /// The warning is derived from the response's own shape, so a reply that
    /// created no task has no outcome to name.
    #[test]
    fn test_task_outcome_is_read_from_the_reported_value() {
        // A `Message`-only reply created no task (SEND_003).
        let message = Message::new(Role::Agent, vec![Part::text("hi")]);
        assert!(
            task_outcome_note(&TaskState::Completed).is_none(),
            "sanity: COMPLETED is silent"
        );
        SendMessageResponse::Message(message.clone()).warn_outcome();
        StreamResponse::Message(message).warn_outcome();

        // A task-bearing response reads the task's state.
        let task = make_fixture_task("t-1", "c-1", TaskState::Failed, "broke");
        assert_eq!(
            task_outcome_note(&task.status.state),
            Some("the agent reported it FAILED")
        );
        SendMessageResponse::Task(task.clone()).warn_outcome();
        StreamResponse::Task(task).warn_outcome();

        // A streamed status update reads the event's state.
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t-2".to_string(),
            context_id: "c-2".to_string(),
            status: TaskStatus {
                state: TaskState::AuthRequired,
                message: None,
                timestamp: None,
            },
            metadata: None,
        })
        .warn_outcome();
    }

    /// clap renders "error: …" then a blank line then a usage block, and
    /// indents continuation lines. The envelope carries one line, so the
    /// first block is joined and the redundant `error: ` prefix dropped —
    /// the envelope already says it is an error.
    #[test]
    fn test_flatten_clap_message() {
        let rendered = "error: unexpected argument '--nope' found\n\nUsage: a2acli [OPTIONS]\n";
        assert_eq!(
            flatten_clap_message(rendered),
            "unexpected argument '--nope' found"
        );

        // Continuation lines are trimmed before joining, so no run of
        // spaces survives into the JSON.
        let rendered = "error: the following required arguments were not provided:\n  <ID>\n\nUsage: a2acli task get <ID>\n";
        assert_eq!(
            flatten_clap_message(rendered),
            "the following required arguments were not provided: <ID>"
        );

        // A message with no blank line, and one with nothing but a usage
        // block, both still produce something.
        assert_eq!(flatten_clap_message("error: bare"), "bare");
        assert_eq!(flatten_clap_message("\n\nUsage: a2acli"), "Usage: a2acli");
    }

    #[test]
    fn test_usage_error_maps_to_the_appendix_d_usage_code() {
        let error = CliError::Usage("unexpected argument '--nope' found".to_string());
        assert_eq!(error.exit_code(), 2);
        let envelope = error.envelope();
        assert_eq!(envelope.error.code, "A2ACLI_ERR_USAGE");
        assert_eq!(envelope.error.message, "unexpected argument '--nope' found");
        assert!(envelope.error.hint.unwrap().contains("--help"));
    }
}
