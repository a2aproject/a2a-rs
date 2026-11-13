// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;

use a2a::A2AError;
use prost::Message;
use slim_rpc::{Context, DEADLINE_KEY, RPC_DIR_KEY, RpcError, STATUS_CODE_KEY};

pub type ServiceParamsMap = HashMap<String, Vec<String>>;

const RPC_ID_KEY: &str = "rpc-id";
const SERVICE_KEY: &str = "service";
const METHOD_KEY: &str = "method";
/// Signature the SLIM session layer (3.4+) puts on every message it sends.
/// slim-session keeps its own constant crate-private.
const PAYLOAD_SIG_KEY: &str = "slim-payload-sig";

pub const A2A_SLIMRPC_SERVICE: &str = "lf.a2a.v1.A2AService";
pub const METHOD_SEND_MESSAGE: &str = "SendMessage";
pub const METHOD_SEND_STREAMING_MESSAGE: &str = "SendStreamingMessage";
pub const METHOD_SEND_LIVE_MESSAGE: &str = "SendLiveMessage";
pub const METHOD_GET_TASK: &str = "GetTask";
pub const METHOD_LIST_TASKS: &str = "ListTasks";
pub const METHOD_CANCEL_TASK: &str = "CancelTask";
pub const METHOD_SUBSCRIBE_TO_TASK: &str = "SubscribeToTask";
pub const METHOD_CREATE_PUSH_CONFIG: &str = "CreateTaskPushNotificationConfig";
pub const METHOD_GET_PUSH_CONFIG: &str = "GetTaskPushNotificationConfig";
pub const METHOD_LIST_PUSH_CONFIGS: &str = "ListTaskPushNotificationConfigs";
pub const METHOD_DELETE_PUSH_CONFIG: &str = "DeleteTaskPushNotificationConfig";
pub const METHOD_GET_EXTENDED_AGENT_CARD: &str = "GetExtendedAgentCard";

/// The `Collaborate` operation (many-to-many messaging on a SLIM group channel;
/// see the SLIMRPC collaborative channel extension spec) lives on its own service,
/// separate from the point-to-point `A2AService`.
pub const A2A_COLLABORATIVE_CHANNEL_SERVICE: &str =
    "experimental.slimrpc.collaborative_channel.v1.CollaborativeChannelService";
pub const METHOD_COLLABORATE: &str = "Collaborate";

/// Key for the sender's SLIM name.
///
/// - On every inbound `Collaborate` message, the SLIMRPC layer sets it in
///   `Message.metadata`, per the collaborative channel spec's
///   message-attribution requirement.
/// - On every request a [`SlimRpcHandler`](crate::SlimRpcHandler) serves, it is
///   a reserved service-params key holding the caller's SLIM name as the
///   session reports it. Caller-supplied values are dropped. The name is the
///   one the caller claimed when it set up the session; for authorization, use
///   [`SLIM_PEER_SUBJECT_METADATA_KEY`], which is verified.
pub const SLIM_SRC_METADATA_KEY: &str = "slim-src";

/// Reserved service-params key holding the verified subject (`sub`) of the
/// caller's identity token, e.g. a `did:key` or a SPIFFE ID. Set only when the
/// session uses MLS, which is what verifies it; absent otherwise. In a group
/// session it is the member that sent the request, not the one that set the
/// session up. Caller-supplied values are dropped.
pub const SLIM_PEER_SUBJECT_METADATA_KEY: &str = "slim-peer-subject";

/// Reserved service-params key holding the verified issuer (`iss`) of the
/// caller's identity token. Set only when the session uses MLS and the token
/// has an issuer. Caller-supplied values are dropped.
pub const SLIM_PEER_ISSUER_METADATA_KEY: &str = "slim-peer-issuer";

/// Service-params keys that carry the caller identity. Only the SLIMRPC layer
/// sets them, from the session; request metadata can't.
const RESERVED_IDENTITY_KEYS: [&str; 3] = [
    SLIM_SRC_METADATA_KEY,
    SLIM_PEER_SUBJECT_METADATA_KEY,
    SLIM_PEER_ISSUER_METADATA_KEY,
];

pub fn encode_proto_message<T>(message: &T) -> Vec<u8>
where
    T: Message,
{
    message.encode_to_vec()
}

pub fn decode_proto_response<T>(bytes: Vec<u8>, type_name: &str) -> Result<T, A2AError>
where
    T: Message + Default,
{
    T::decode(bytes.as_slice())
        .map_err(|error| A2AError::internal(format!("invalid {type_name} payload: {error}")))
}

pub fn decode_proto_request<T>(bytes: Vec<u8>, type_name: &str) -> Result<T, RpcError>
where
    T: Message + Default,
{
    T::decode(bytes.as_slice()).map_err(|error| {
        RpcError::invalid_argument(format!("invalid {type_name} payload: {error}"))
    })
}

pub fn service_params_to_metadata(params: &ServiceParamsMap) -> HashMap<String, String> {
    params
        .iter()
        .map(|(key, values)| (key.clone(), values.join(", ")))
        .collect()
}

pub fn service_params_to_metadata_opt(
    params: &ServiceParamsMap,
) -> Option<HashMap<String, String>> {
    let metadata = service_params_to_metadata(params);
    if metadata.is_empty() {
        None
    } else {
        Some(metadata)
    }
}

/// The caller identity a server handler sees, taken from the session rather
/// than from request metadata.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CallerIdentity {
    pub slim_src: Option<String>,
    pub peer_subject: Option<String>,
    pub peer_issuer: Option<String>,
}

impl CallerIdentity {
    pub fn from_context(context: &Context) -> Self {
        // In a handler, the session's destination is the caller. A context
        // built without a session has an empty name, which isn't forwarded.
        let caller = context.session().destination();
        let (org, namespace, app) = caller.str_components();
        let slim_src = (!(org.is_empty() && namespace.is_empty() && app.is_empty()))
            .then(|| caller.to_string());
        Self {
            slim_src,
            peer_subject: context.peer_subject(),
            peer_issuer: context.peer_issuer(),
        }
    }
}

/// Build the service params a server handler sees: the request metadata minus
/// internal keys and caller-supplied reserved identity keys, plus the caller
/// identity from the session under those reserved keys.
pub fn context_to_service_params(context: &Context) -> ServiceParamsMap {
    to_service_params(context.metadata(), CallerIdentity::from_context(context))
}

fn to_service_params(
    metadata: HashMap<String, String>,
    identity: CallerIdentity,
) -> ServiceParamsMap {
    let mut params: ServiceParamsMap = metadata
        .into_iter()
        .filter(|(key, _)| !is_internal_metadata_key(key) && !is_reserved_identity_key(key))
        .map(|(key, value)| (key, vec![value]))
        .collect();
    let identity_params = [
        (SLIM_SRC_METADATA_KEY, identity.slim_src),
        (SLIM_PEER_SUBJECT_METADATA_KEY, identity.peer_subject),
        (SLIM_PEER_ISSUER_METADATA_KEY, identity.peer_issuer),
    ];
    for (key, value) in identity_params {
        if let Some(value) = value {
            params.insert(key.to_string(), vec![value]);
        }
    }
    params
}

fn is_reserved_identity_key(key: &str) -> bool {
    // Metadata keys are header-like, so a differently-cased copy must not slip
    // past a handler that normalizes case.
    RESERVED_IDENTITY_KEYS
        .iter()
        .any(|reserved| key.eq_ignore_ascii_case(reserved))
}

fn is_internal_metadata_key(key: &str) -> bool {
    matches!(
        key,
        DEADLINE_KEY | STATUS_CODE_KEY | RPC_ID_KEY | SERVICE_KEY | METHOD_KEY | RPC_DIR_KEY
    ) || key.eq_ignore_ascii_case(PAYLOAD_SIG_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_params_to_metadata() {
        let mut params = ServiceParamsMap::new();
        params.insert("x-single".to_string(), vec!["one".to_string()]);
        params.insert(
            "x-multi".to_string(),
            vec!["one".to_string(), "two".to_string()],
        );

        let metadata = service_params_to_metadata(&params);
        assert_eq!(metadata.get("x-single"), Some(&"one".to_string()));
        assert_eq!(metadata.get("x-multi"), Some(&"one, two".to_string()));
    }

    #[test]
    fn test_service_params_to_metadata_opt_empty() {
        assert!(service_params_to_metadata_opt(&ServiceParamsMap::new()).is_none());
    }

    #[test]
    fn test_service_params_to_metadata_opt_non_empty() {
        let params = ServiceParamsMap::from([(
            "x-test".to_string(),
            vec!["one".to_string(), "two".to_string()],
        )]);

        let metadata = service_params_to_metadata_opt(&params).unwrap();
        assert_eq!(metadata.get("x-test"), Some(&"one, two".to_string()));
    }

    #[test]
    fn test_context_to_service_params_filters_internal_metadata() {
        let context =
            Context::for_rpc(None, None).with_message_metadata(std::collections::HashMap::from([
                (DEADLINE_KEY.to_string(), "1".to_string()),
                (STATUS_CODE_KEY.to_string(), "0".to_string()),
                ("rpc-id".to_string(), "abc".to_string()),
                ("service".to_string(), "svc".to_string()),
                ("method".to_string(), "m".to_string()),
                (RPC_DIR_KEY.to_string(), "req".to_string()),
                ("slim-payload-sig".to_string(), "MEQCIF".to_string()),
                ("Slim-Payload-Sig".to_string(), "MEQCIF".to_string()),
                ("x-user".to_string(), "value".to_string()),
            ]));

        let params = context_to_service_params(&context);
        assert_eq!(params.get("x-user"), Some(&vec!["value".to_string()]));
        assert!(!params.contains_key(DEADLINE_KEY));
        assert!(!params.contains_key(STATUS_CODE_KEY));
        assert!(!params.contains_key("rpc-id"));
        assert!(!params.contains_key("service"));
        assert!(!params.contains_key("method"));
        assert!(!params.contains_key(RPC_DIR_KEY));
        assert!(!params.contains_key("slim-payload-sig"));
        assert!(!params.contains_key("Slim-Payload-Sig"));
    }

    #[test]
    fn test_context_to_service_params_strips_spoofed_identity_keys() {
        // Plain request metadata, which is all a caller controls. Not
        // `with_message_metadata`: since slim-rpc 3.4 that is where the
        // session layer's MLS-stamped sender enters (see the next test), and
        // the session layer strips a caller's own value before it gets there.
        let context = Context::for_rpc(
            None,
            Some(std::collections::HashMap::from([
                (
                    SLIM_SRC_METADATA_KEY.to_string(),
                    "org/evil/app".to_string(),
                ),
                ("Slim-Src".to_string(), "org/evil/app".to_string()),
                (
                    SLIM_PEER_SUBJECT_METADATA_KEY.to_string(),
                    "did:key:spoofed".to_string(),
                ),
                (
                    "SLIM-PEER-ISSUER".to_string(),
                    "https://evil.example".to_string(),
                ),
                ("x-user".to_string(), "value".to_string()),
            ])),
        );

        // A context without a session carries no caller identity, so nothing
        // may come back under the reserved keys, in any case.
        let params = context_to_service_params(&context);
        assert_eq!(params.get("x-user"), Some(&vec!["value".to_string()]));
        assert!(
            params.keys().all(|key| !is_reserved_identity_key(key)),
            "reserved keys leaked: {params:?}"
        );
    }

    #[test]
    fn test_context_to_service_params_forwards_the_mls_sender_subject() {
        // slim-rpc 3.4 reads the subject its session layer stamps from the MLS
        // sender into the message metadata, so a handler sees who sent this
        // request. It reaches the handler exactly once, under the reserved key.
        let context =
            Context::for_rpc(None, None).with_message_metadata(std::collections::HashMap::from([
                (
                    SLIM_PEER_SUBJECT_METADATA_KEY.to_string(),
                    "did:key:z6Mk".to_string(),
                ),
                ("x-user".to_string(), "value".to_string()),
            ]));

        let params = context_to_service_params(&context);
        assert_eq!(
            params.get(SLIM_PEER_SUBJECT_METADATA_KEY),
            Some(&vec!["did:key:z6Mk".to_string()])
        );
        assert_eq!(params.get("x-user"), Some(&vec!["value".to_string()]));
    }

    #[test]
    fn test_to_service_params_populates_identity() {
        let metadata = HashMap::from([
            (
                SLIM_SRC_METADATA_KEY.to_string(),
                "org/evil/app".to_string(),
            ),
            (
                SLIM_PEER_SUBJECT_METADATA_KEY.to_string(),
                "did:key:spoofed".to_string(),
            ),
            ("x-user".to_string(), "value".to_string()),
        ]);
        let identity = CallerIdentity {
            slim_src: Some("org/ns/caller/42".to_string()),
            peer_subject: Some("did:key:z6Mk".to_string()),
            peer_issuer: Some("https://issuer.example".to_string()),
        };

        let params = to_service_params(metadata, identity);
        assert_eq!(
            params.get(SLIM_SRC_METADATA_KEY),
            Some(&vec!["org/ns/caller/42".to_string()])
        );
        assert_eq!(
            params.get(SLIM_PEER_SUBJECT_METADATA_KEY),
            Some(&vec!["did:key:z6Mk".to_string()])
        );
        assert_eq!(
            params.get(SLIM_PEER_ISSUER_METADATA_KEY),
            Some(&vec!["https://issuer.example".to_string()])
        );
        assert_eq!(params.get("x-user"), Some(&vec!["value".to_string()]));
    }

    #[test]
    fn test_to_service_params_omits_unset_identity() {
        let metadata = HashMap::from([(
            SLIM_PEER_ISSUER_METADATA_KEY.to_string(),
            "https://evil.example".to_string(),
        )]);
        let identity = CallerIdentity {
            slim_src: Some("org/ns/caller/42".to_string()),
            peer_subject: Some("shared-secret-id".to_string()),
            peer_issuer: None,
        };

        let params = to_service_params(metadata, identity);
        assert!(params.contains_key(SLIM_SRC_METADATA_KEY));
        assert!(params.contains_key(SLIM_PEER_SUBJECT_METADATA_KEY));
        assert!(!params.contains_key(SLIM_PEER_ISSUER_METADATA_KEY));
    }

    #[test]
    fn test_caller_identity_from_context_without_session() {
        assert_eq!(
            CallerIdentity::from_context(&Context::for_rpc(None, None)),
            CallerIdentity::default()
        );
    }

    #[test]
    fn test_decode_proto_response_invalid_payload() {
        let error = decode_proto_response::<a2a_pb::proto::Task>(vec![0xff], "Task").unwrap_err();
        assert_eq!(error.code, a2a::error_code::INTERNAL_ERROR);
        assert!(error.message.contains("invalid Task payload"));
    }

    #[test]
    fn test_decode_proto_request_invalid_payload() {
        let error = decode_proto_request::<a2a_pb::proto::Task>(vec![0xff], "Task").unwrap_err();
        assert_eq!(error.code(), slim_rpc::RpcCode::InvalidArgument);
        assert!(error.message().contains("invalid Task payload"));
    }
}
