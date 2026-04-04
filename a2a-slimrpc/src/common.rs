// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;

use a2a::A2AError;
use prost::Message;
use slim_bindings::{Context, DEADLINE_KEY, RpcError, STATUS_CODE_KEY};

pub type ServiceParamsMap = HashMap<String, Vec<String>>;

const RPC_ID_KEY: &str = "rpc-id";
const SERVICE_KEY: &str = "service";
const METHOD_KEY: &str = "method";

pub const A2A_SLIMRPC_SERVICE: &str = "lf.a2a.v1.A2AService";
pub const METHOD_SEND_MESSAGE: &str = "SendMessage";
pub const METHOD_SEND_STREAMING_MESSAGE: &str = "SendStreamingMessage";
pub const METHOD_GET_TASK: &str = "GetTask";
pub const METHOD_LIST_TASKS: &str = "ListTasks";
pub const METHOD_CANCEL_TASK: &str = "CancelTask";
pub const METHOD_SUBSCRIBE_TO_TASK: &str = "SubscribeToTask";
pub const METHOD_CREATE_PUSH_CONFIG: &str = "CreateTaskPushNotificationConfig";
pub const METHOD_GET_PUSH_CONFIG: &str = "GetTaskPushNotificationConfig";
pub const METHOD_LIST_PUSH_CONFIGS: &str = "ListTaskPushNotificationConfigs";
pub const METHOD_DELETE_PUSH_CONFIG: &str = "DeleteTaskPushNotificationConfig";
pub const METHOD_GET_EXTENDED_AGENT_CARD: &str = "GetExtendedAgentCard";

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

pub fn context_to_service_params(context: &Context) -> ServiceParamsMap {
    context
        .metadata()
        .into_iter()
        .filter(|(key, _)| !is_internal_metadata_key(key))
        .map(|(key, value)| (key, vec![value]))
        .collect()
}

fn is_internal_metadata_key(key: &str) -> bool {
    matches!(
        key,
        DEADLINE_KEY | STATUS_CODE_KEY | RPC_ID_KEY | SERVICE_KEY | METHOD_KEY
    )
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
                ("x-user".to_string(), "value".to_string()),
            ]));

        let params = context_to_service_params(&context);
        assert_eq!(params.get("x-user"), Some(&vec!["value".to_string()]));
        assert!(!params.contains_key(DEADLINE_KEY));
        assert!(!params.contains_key(STATUS_CODE_KEY));
        assert!(!params.contains_key("rpc-id"));
        assert!(!params.contains_key("service"));
        assert!(!params.contains_key("method"));
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
        assert_eq!(error.code(), slim_bindings::RpcCode::InvalidArgument);
        assert!(error.message().contains("invalid Task payload"));
    }
}
