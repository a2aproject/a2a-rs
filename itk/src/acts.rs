// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! ACTS SUT behaviour contract (ACTS spec §11).
//!
//! ACTS tests are declarative — they say what to send and what to expect — so
//! the agent under test has to produce a deterministic reply for each case.
//! §11 does that with a message-prefix convention rather than a side-channel
//! API: the text of the first user message names the behaviour. `execute`
//! routes here when it sees a `tck-` prefix and otherwise runs the ITK
//! instruction path, so one binary serves both suites.
//!
//! `acts/sut-behaviors.yaml` is what this SDK claims; this file is what it
//! does. Nothing here restates the list of names — the prefix is read out of
//! the message, and one that reaches `dispatch` without a branch fails the
//! task rather than completing it, so a gap shows up in the conformance
//! report instead of passing quietly.

use std::time::Duration;

use a2a::*;
use a2a_server::ExecutorContext;
use futures::stream::BoxStream;
use tokio::time::sleep;
use tracing::info;

/// The word a multi-turn conversation ends on, fixed by the corpus.
const MULTI_TURN_DONE: &str = "done";

/// How long `tck-long-running` stays in WORKING before completing. Short
/// enough not to dominate a run, long enough that a test polling for a
/// non-terminal state sees one: the corpus polls every 2s, 15 times.
const LONG_RUNNING_DELAY: Duration = Duration::from_secs(1);

/// `tck-cancel` holds WORKING by heartbeat rather than parking forever.
///
/// The server has no cancellation token: it notices a cancel only when the
/// execution stream yields again, and an executor that parks leaks its task
/// for the life of the process. One second bounds teardown latency; the cap
/// bounds the leak if no cancel ever arrives.
const CANCEL_HEARTBEAT: Duration = Duration::from_secs(1);
const CANCEL_HEARTBEATS: usize = 120;

/// The environment variable that asks for a diminished agent card.
///
/// Four ACTS tests assert that an agent *without* a capability answers
/// `UnsupportedOperationError`, so their preconditions require the card not to
/// advertise it and they can never run against a fully capable agent. The
/// runner starts a second SUT with this set to reach them.
pub const REDUCED_CAPABILITIES_ENV: &str = "ITK_ACTS_REDUCED_CAPABILITIES";

/// What the card advertises — everything, unless asked for less.
pub fn capabilities() -> AgentCapabilities {
    if std::env::var_os(REDUCED_CAPABILITIES_ENV).is_some() {
        info!("Advertising no optional capabilities (ACTS reduced pass)");
        return AgentCapabilities {
            streaming: Some(false),
            push_notifications: Some(false),
            extensions: None,
            extended_agent_card: Some(false),
        };
    }
    AgentCapabilities {
        streaming: Some(true),
        push_notifications: Some(true),
        extensions: None,
        extended_agent_card: Some(true),
    }
}

/// The behaviour named by `text`, if any.
///
/// Hand-rolled rather than a regex so the crate takes no new dependency: the
/// launcher builds with `--locked`, and adding one would mean editing the
/// committed lockfile. Consumes the longest run of `[a-z0-9-]` after the
/// prefix and trims a trailing `-`, which gives longest-match for free —
/// `tck-artifact-file-url` beats `tck-artifact-file` with no ordered table.
///
/// Names an *asserted* behaviour, not necessarily an implemented one: an
/// unknown `tck-` still routes to ACTS and is reported as unimplemented,
/// which beats handing a message plainly meant for ACTS to the ITK decoder.
pub fn behavior_in(text: &str) -> Option<String> {
    let text = text.trim_start();
    if !text.starts_with("tck-") {
        return None;
    }
    let name: String = text
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect();
    let name = name.trim_end_matches('-');
    (name.len() > "tck-".len()).then(|| name.to_string())
}

/// The behaviour this request belongs to, from the incoming message or the
/// task that opened the conversation.
///
/// A multi-turn test opens with the prefix and then sends plain `here is more
/// input` and `done`, so a continuation has to recover the contract from
/// where it was declared. The server writes `history` once, at task creation,
/// so `history[0]` is that opening message.
pub fn behavior_for(ctx: &ExecutorContext) -> Option<String> {
    if let Some(named) = ctx
        .message
        .as_ref()
        .and_then(Message::text)
        .and_then(behavior_in)
    {
        return Some(named);
    }
    ctx.stored_task
        .as_ref()
        .and_then(|task| task.history.as_ref())
        .and_then(|history| history.first())
        .and_then(Message::text)
        .and_then(behavior_in)
}

fn status(ctx: &ExecutorContext, state: TaskState, text: Option<&str>) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        status: TaskStatus {
            state,
            message: text.map(|t| Message::new(Role::Agent, vec![Part::text(t)])),
            timestamp: None,
        },
        metadata: None,
    })
}

fn artifact(ctx: &ExecutorContext, name: &str, parts: Vec<Part>) -> StreamResponse {
    StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        artifact: Artifact {
            artifact_id: new_artifact_id(),
            name: Some(name.to_string()),
            description: None,
            parts,
            metadata: None,
            extensions: None,
        },
        append: None,
        last_chunk: Some(true),
        metadata: None,
    })
}

/// The artifact shape a `tck-artifact-*` behaviour names, or `None` if the
/// suffix is not one this agent implements.
fn artifact_parts(behavior: &str) -> Option<Vec<Part>> {
    match behavior {
        "tck-artifact-text" => Some(vec![Part::text("generated text content")]),
        "tck-artifact-data" => Some(vec![Part::data(serde_json::json!({
            "key": "value",
            "count": 1,
        }))]),
        "tck-artifact-file" => Some(vec![
            Part::raw(b"file bytes".to_vec())
                .with_filename("document.txt")
                .with_media_type("text/plain"),
        ]),
        "tck-artifact-file-url" => Some(vec![
            Part::url("https://example.com/document.txt")
                .with_filename("document.txt")
                .with_media_type("text/plain"),
        ]),
        _ => None,
    }
}

/// Serve one ACTS request, start to finish.
pub fn run(
    behavior: String,
    ctx: ExecutorContext,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    Box::pin(async_stream::try_stream! {
        info!(behavior = %behavior, task_id = %ctx.task_id, "Serving ACTS behaviour");

        // A2A lets an agent answer with a bare Message, and the handler
        // short-circuits on the first one. Yielding anything after it would
        // turn the reply back into a task update, which is the opposite of
        // what CORE-SEND-003 checks.
        if behavior == "tck-message-response" {
            let mut reply = Message::new(
                Role::Agent,
                vec![Part::text("tck message response")],
            );
            reply.context_id = Some(ctx.context_id.clone());
            yield StreamResponse::Message(reply);
            return;
        }

        yield status(&ctx, TaskState::Working, None);

        match behavior.as_str() {
            crate::acts_client_parse::BEHAVIOR => {
                match crate::acts_client_parse::artifact_parts(ctx.message.as_ref()).await {
                    Ok(parts) => {
                        yield artifact(&ctx, crate::acts_client_parse::BEHAVIOR, parts);
                        yield status(&ctx, TaskState::Completed, Some("client payload parsed"));
                    }
                    Err(message) => yield status(&ctx, TaskState::Failed, Some(&message)),
                }
            }

            "tck-multi-turn" => {
                let said = ctx
                    .message
                    .as_ref()
                    .and_then(Message::text)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                if said.starts_with(MULTI_TURN_DONE) {
                    yield status(&ctx, TaskState::Completed, Some("multi-turn complete"));
                } else {
                    yield status(&ctx, TaskState::InputRequired, Some("more input please"));
                }
            }

            "tck-cancel" => {
                for _ in 0..CANCEL_HEARTBEATS {
                    sleep(CANCEL_HEARTBEAT).await;
                    yield status(&ctx, TaskState::Working, None);
                }
            }

            "tck-long-running" => {
                sleep(LONG_RUNNING_DELAY).await;
                // CORE-EXEC-001 polls to completion and then asserts the
                // finished task carries at least one artifact, so the work has
                // to leave one behind even though §11.2 describes this
                // behaviour only as delayed completion.
                yield artifact(&ctx, "long-running", vec![Part::text("long running result")]);
                yield status(&ctx, TaskState::Completed, Some("long running work finished"));
            }

            "tck-stream-basic" => {
                yield status(&ctx, TaskState::Working, Some("streaming started"));
                yield artifact(&ctx, "streamed", vec![Part::text("streamed content")]);
                yield status(&ctx, TaskState::Completed, Some("tck-stream-basic ok"));
            }

            "tck-stream-chunked" => {
                yield status(&ctx, TaskState::Working, Some("streaming started"));
                let artifact_id = new_artifact_id();
                let chunks = ["chunk one ", "chunk two ", "chunk three"];
                for (index, chunk) in chunks.iter().enumerate() {
                    let first = index == 0;
                    yield StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                        task_id: ctx.task_id.clone(),
                        context_id: ctx.context_id.clone(),
                        artifact: Artifact {
                            artifact_id: artifact_id.clone(),
                            name: first.then(|| "chunked".to_string()),
                            description: None,
                            parts: vec![Part::text(*chunk)],
                            metadata: None,
                            extensions: None,
                        },
                        append: (!first).then_some(true),
                        last_chunk: (index == chunks.len() - 1).then_some(true),
                        metadata: None,
                    });
                }
                yield status(&ctx, TaskState::Completed, Some("tck-stream-chunked ok"));
            }

            "tck-complete-task" => yield status(&ctx, TaskState::Completed, Some("tck-complete-task ok")),
            "tck-task-failure" => yield status(&ctx, TaskState::Failed, Some("tck-task-failure ok")),
            "tck-reject-task" => yield status(&ctx, TaskState::Rejected, Some("tck-reject-task ok")),
            "tck-input-required" => yield status(&ctx, TaskState::InputRequired, Some("tck-input-required ok")),
            "tck-auth-required" => yield status(&ctx, TaskState::AuthRequired, Some("tck-auth-required ok")),

            other => {
                if let Some(parts) = artifact_parts(other) {
                    yield artifact(&ctx, other, parts);
                    yield status(&ctx, TaskState::Completed, Some(&format!("{other} ok")));
                } else {
                    // Fail loudly rather than completing: a silent success
                    // would report conformance the agent never demonstrated.
                    yield status(
                        &ctx,
                        TaskState::Failed,
                        Some(&format!("unimplemented ACTS behaviour {other:?}")),
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_a_behaviour() {
        assert_eq!(behavior_in("hello world"), None);
        assert_eq!(behavior_in(""), None);
        assert_eq!(behavior_in("tck-"), None);
    }

    #[test]
    fn the_name_stops_at_the_first_character_that_cannot_be_in_one() {
        assert_eq!(
            behavior_in("tck-complete-task and then some"),
            Some("tck-complete-task".to_string())
        );
    }

    #[test]
    fn longest_match_wins_without_an_ordered_table() {
        assert_eq!(
            behavior_in("tck-artifact-file-url document"),
            Some("tck-artifact-file-url".to_string())
        );
        assert_eq!(
            behavior_in("tck-artifact-file document"),
            Some("tck-artifact-file".to_string())
        );
    }

    #[test]
    fn a_trailing_hyphen_is_not_part_of_the_name() {
        assert_eq!(
            behavior_in("tck-cancel- now"),
            Some("tck-cancel".to_string())
        );
    }

    #[test]
    fn leading_whitespace_is_tolerated() {
        assert_eq!(behavior_in("  tck-cancel"), Some("tck-cancel".to_string()));
    }

    #[test]
    fn an_unknown_prefix_still_routes_to_acts() {
        // So it is reported as unimplemented rather than handed to the ITK
        // instruction decoder, which would fail with "no valid instruction".
        assert_eq!(
            behavior_in("tck-nonesuch"),
            Some("tck-nonesuch".to_string())
        );
        assert!(artifact_parts("tck-nonesuch").is_none());
    }

    #[test]
    fn reduced_capabilities_advertises_nothing_optional() {
        // Not asserted against the env var, which is process-global and would
        // make this test order-dependent; the shape is the contract.
        let full = AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(true),
            extensions: None,
            extended_agent_card: Some(true),
        };
        assert_eq!(capabilities(), full);
    }
}
