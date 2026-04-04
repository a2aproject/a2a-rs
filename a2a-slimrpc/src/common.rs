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
}
