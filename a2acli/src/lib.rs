// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use a2a::*;
use a2a_client::auth::AuthInterceptor;
use a2a_client::{A2AClient, A2AClientFactory, BoxStream};
use clap::{ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use futures::StreamExt;
use reqwest::{Client, RequestBuilder};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(name = "a2acli", version, about = "Standalone A2A client CLI")]
pub struct Cli {
    /// Base URL used to resolve /.well-known/agent-card.json.
    #[arg(long, global = true, default_value = "http://localhost:3000")]
    pub base_url: String,

    /// Prefer a specific transport when the agent card exposes multiple bindings.
    #[arg(long, global = true, value_enum)]
    pub binding: Option<Binding>,

    /// Bearer token attached to the agent-card fetch and client calls.
    #[arg(long, global = true, env = "A2A_BEARER_TOKEN")]
    pub bearer_token: Option<String>,

    /// Extra HTTP header attached to the agent-card fetch and client calls.
    #[arg(long = "header", global = true, value_parser = parse_header)]
    pub headers: Vec<HeaderArg>,

    /// Optional tenant forwarded to A2A requests that support it.
    #[arg(long, global = true)]
    pub tenant: Option<String>,

    /// Emit compact JSON instead of pretty-printed JSON.
    #[arg(long, global = true)]
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
        conflicts_with = "wait"
    )]
    pub async_mode: bool,

    /// Explicitly block until the task reaches a terminal or interrupted state.
    /// This is already the default for `send`; on `task get` it turns the
    /// one-shot read into a poll loop.
    #[arg(long, global = true, conflicts_with = "async_mode")]
    pub wait: bool,

    /// Delay between polls while waiting for a task to settle (e.g. "2s", "500ms").
    #[arg(long, global = true, value_parser = parse_duration, default_value = "2s")]
    pub poll_interval: Duration,

    /// Overall time budget for a blocking wait before reporting a timeout (e.g. "30s", "2m").
    #[arg(long, global = true, value_parser = parse_duration, default_value = "30s")]
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
    /// Send a message to start or continue an interaction.
    Send(MessageCommand),
    /// Task operations.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum CardCommand {
    /// Fetch and print the Agent Card. Pass --extended for the authenticated extended card.
    Get(CardGetCommand),
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct CardGetCommand {
    /// Fetch the authenticated extended agent card instead of the public one.
    #[arg(long)]
    pub extended: bool,
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
    #[arg(long)]
    pub context_id: Option<String>,

    /// Optional task identifier to continue an existing task.
    #[arg(long)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Binding {
    Jsonrpc,
    HttpJson,
}

impl Binding {
    fn protocol(self) -> &'static str {
        match self {
            Binding::Jsonrpc => TRANSPORT_PROTOCOL_JSONRPC,
            Binding::HttpJson => TRANSPORT_PROTOCOL_HTTP_JSON,
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
}

/// Parse `args` the same way the real binary parses `std::env::args_os()`, then
/// run the resulting command. Kept separate from [`run`] because recovering
/// the interleaved order of `send`'s repeatable part flags
/// (`--text-part`/`--file-part`/`--data-part`/`--media-type`) needs the raw
/// [`ArgMatches`], which `Cli::parse()` alone discards.
pub async fn run_args<I, T>(args: I) -> Result<(), CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = Cli::command().get_matches_from(args);
    let cli = Cli::from_arg_matches(&matches).expect("matches were produced by Cli::command()");
    run(cli, &matches).await
}

pub async fn run(cli: Cli, matches: &ArgMatches) -> Result<(), CliError> {
    match &cli.command {
        Command::Card { command } => run_card_command(&cli, command).await?,
        Command::Send(command) => {
            let send_matches = matches
                .subcommand_matches("send")
                .expect("send subcommand matches present when cli.command is Command::Send");
            let parts = resolve_message_parts(send_matches, command)?;
            let request = build_send_message_request(command, parts, cli.tenant.clone());
            let client = resolve_client(&cli).await?;

            if command.stream {
                match client.send_streaming_message(&request).await {
                    Ok(stream) => {
                        consume_stream(client, stream, cli.compact).await?;
                    }
                    Err(error) if error.code == a2a::error_code::UNSUPPORTED_OPERATION => {
                        // The agent doesn't advertise the streaming
                        // capability — fall back to a one-shot send plus
                        // polling rather than hanging (TASK_POLL_004). Any
                        // *other* error opening the stream is a real
                        // failure and must be reported as such, not masked
                        // by a silent retry.
                        let response = send_and_maybe_wait(client, &request, &cli).await?;
                        print_json(&response, cli.compact)?;
                    }
                    Err(error) => {
                        let _ = client.destroy().await;
                        return Err(error.into());
                    }
                }
            } else {
                let response = send_and_maybe_wait(client, &request, &cli).await?;
                print_json(&response, cli.compact)?;
            }
        }
        Command::Task { command } => run_task_command(&cli, command).await?,
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
    cli: &Cli,
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
            let task = settle_task(
                client,
                task,
                !cli.async_mode,
                cli.tenant.clone(),
                cli.poll_interval,
                cli.timeout,
            )
            .await?;
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

/// A task in a terminal (`COMPLETED`/`FAILED`/`CANCELED`/`REJECTED`) or
/// interrupted (`INPUT_REQUIRED`/`AUTH_REQUIRED`) state needs no more polling.
fn is_settled(state: &TaskState) -> bool {
    state.is_terminal() || matches!(state, TaskState::InputRequired | TaskState::AuthRequired)
}

async fn run_card_command(cli: &Cli, command: &CardCommand) -> Result<(), CliError> {
    match command {
        CardCommand::Get(command) => {
            if command.extended {
                let client = resolve_client(cli).await?;
                let result = client
                    .get_extended_agent_card(&GetExtendedAgentCardRequest {
                        tenant: cli.tenant.clone(),
                    })
                    .await;
                let card = finish_client_call(client, result).await?;
                print_json(&card, cli.compact)?;
            } else {
                let card = resolve_agent_card(cli).await?;
                print_json(&card, cli.compact)?;
            }
        }
    }

    Ok(())
}

async fn run_task_command(cli: &Cli, command: &TaskCommand) -> Result<(), CliError> {
    match command {
        TaskCommand::Get(command) => {
            let client = resolve_client(cli).await?;
            let result = client
                .get_task(&GetTaskRequest {
                    id: command.id.clone(),
                    history_length: command.history_length,
                    tenant: cli.tenant.clone(),
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
                cli.tenant.clone(),
                cli.poll_interval,
                cli.timeout,
            )
            .await?;
            print_json(&task, cli.compact)?;
        }
        TaskCommand::List(command) => {
            let client = resolve_client(cli).await?;
            let result = client
                .list_tasks(&ListTasksRequest {
                    context_id: command.context_id.clone(),
                    status: command.status.map(TaskState::from),
                    page_size: command.page_size,
                    page_token: command.page_token.clone(),
                    history_length: command.history_length,
                    status_timestamp_after: None,
                    include_artifacts: command.include_artifacts.then_some(true),
                    tenant: cli.tenant.clone(),
                })
                .await;
            let response = finish_client_call(client, result).await?;
            print_json(&response, cli.compact)?;
        }
        TaskCommand::Cancel(command) => {
            let client = resolve_client(cli).await?;
            let result = client
                .cancel_task(&CancelTaskRequest {
                    id: command.id.clone(),
                    metadata: None,
                    tenant: cli.tenant.clone(),
                })
                .await;
            let task = finish_client_call(client, result).await?;
            print_json(&task, cli.compact)?;
        }
        TaskCommand::Subscribe(command) => {
            let client = resolve_client(cli).await?;
            let stream = client
                .subscribe_to_task(&SubscribeToTaskRequest {
                    id: command.id.clone(),
                    tenant: cli.tenant.clone(),
                })
                .await?;
            consume_stream(client, stream, cli.compact).await?;
        }
        TaskCommand::PushConfig { command } => {
            run_push_config_command(cli, command).await?;
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
        return Ok(Part::data(serde_json::from_str(&buffer)?));
    }

    match std::fs::read_to_string(value) {
        Ok(text) => return Ok(Part::data(serde_json::from_str(&text)?)),
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

async fn run_push_config_command(cli: &Cli, command: &PushConfigCommand) -> Result<(), CliError> {
    match command {
        PushConfigCommand::Create(command) => {
            let client = resolve_client(cli).await?;
            let mut config = build_push_notification_config(command)?;
            config.task_id = command.task_id.clone();
            config.tenant = cli.tenant.clone();
            let result = client.create_push_config(&config).await;
            let response = finish_client_call(client, result).await?;
            print_json(&response, cli.compact)?;
        }
        PushConfigCommand::Get(command) => {
            let client = resolve_client(cli).await?;
            let result = client
                .get_push_config(&GetTaskPushNotificationConfigRequest {
                    task_id: command.task_id.clone(),
                    id: command.id.clone(),
                    tenant: cli.tenant.clone(),
                })
                .await;
            let response = finish_client_call(client, result).await?;
            print_json(&response, cli.compact)?;
        }
        PushConfigCommand::List(command) => {
            let client = resolve_client(cli).await?;
            let result = client
                .list_push_configs(&ListTaskPushNotificationConfigsRequest {
                    task_id: command.task_id.clone(),
                    page_size: command.page_size,
                    page_token: command.page_token.clone(),
                    tenant: cli.tenant.clone(),
                })
                .await;
            let response = finish_client_call(client, result).await?;
            print_json(&response, cli.compact)?;
        }
        PushConfigCommand::Delete(command) => {
            let client = resolve_client(cli).await?;
            let result = client
                .delete_push_config(&DeleteTaskPushNotificationConfigRequest {
                    task_id: command.task_id.clone(),
                    id: command.id.clone(),
                    tenant: cli.tenant.clone(),
                })
                .await;
            finish_client_call(client, result).await?;
            print_json(
                &serde_json::json!({
                    "deleted": true,
                    "taskId": command.task_id,
                    "id": command.id,
                }),
                cli.compact,
            )?;
        }
    }

    Ok(())
}

async fn resolve_client(cli: &Cli) -> Result<A2AClient<Box<dyn a2a_client::Transport>>, CliError> {
    let card = resolve_agent_card(cli).await?;

    let mut builder = A2AClientFactory::builder();
    if let Some(binding) = cli.binding {
        builder = builder.preferred_bindings(vec![binding.protocol().to_string()]);
    }
    if let Some(token) = &cli.bearer_token {
        builder = builder.with_interceptor(Arc::new(AuthInterceptor::bearer(token.clone())));
    }
    for header in &cli.headers {
        builder = builder.with_interceptor(Arc::new(AuthInterceptor::custom(
            header.name.clone(),
            header.value.clone(),
        )));
    }

    let factory = builder.build();
    Ok(factory.create_from_card(&card).await?)
}

async fn resolve_agent_card(cli: &Cli) -> Result<AgentCard, CliError> {
    let url = format!(
        "{}/.well-known/agent-card.json",
        cli.base_url.trim_end_matches('/')
    );
    let client = Client::new();
    let request = apply_request_auth(client.get(url), cli);
    let response = request.send().await?.error_for_status()?;
    Ok(response.json::<AgentCard>().await?)
}

fn apply_request_auth(mut request: RequestBuilder, cli: &Cli) -> RequestBuilder {
    if let Some(token) = &cli.bearer_token {
        request = request.bearer_auth(token);
    }
    for header in &cli.headers {
        request = request.header(&header.name, &header.value);
    }
    request
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

async fn consume_stream<T: a2a_client::Transport, V: Serialize>(
    client: A2AClient<T>,
    mut stream: BoxStream<'static, Result<V, A2AError>>,
    compact: bool,
) -> Result<(), CliError> {
    loop {
        match stream.next().await {
            Some(Ok(value)) => {
                if let Err(error) = print_json(&value, compact) {
                    let _ = client.destroy().await;
                    return Err(error);
                }
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
            "--base-url".to_string(),
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
            "--binding",
            "jsonrpc",
            "--header",
            "X-Test:123",
            "send",
            "hello",
            "--history-length",
            "2",
        ])
        .unwrap();

        assert_eq!(cli.binding, Some(Binding::Jsonrpc));
        assert_eq!(cli.headers.len(), 1);
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
        assert_eq!(Binding::HttpJson.protocol(), TRANSPORT_PROTOCOL_HTTP_JSON);
    }

    #[test]
    fn test_apply_request_auth_builds_headers() {
        let cli = Cli::try_parse_from([
            "a2acli",
            "--bearer-token",
            "secret",
            "--header",
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

    #[tokio::test]
    async fn test_consume_stream_reports_json_error() {
        let stream = Box::pin(stream::once(async { Ok(FailingSerialize) }));
        let err = consume_stream(make_test_client(None), stream, false)
            .await
            .unwrap_err();

        assert!(matches!(err, CliError::Json(_)));
    }

    #[tokio::test]
    async fn test_consume_stream_propagates_destroy_error_on_completion() {
        let stream = Box::pin(stream::empty::<Result<serde_json::Value, A2AError>>());
        let err = consume_stream(
            make_test_client(Some(A2AError::internal("destroy failed"))),
            stream,
            true,
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
                "--binding",
                "jsonrpc",
                "--bearer-token",
                "secret",
                "--header",
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
}
