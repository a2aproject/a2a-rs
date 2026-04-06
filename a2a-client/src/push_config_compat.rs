// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use a2a_pb::protojson_conv;
use serde_json::Value;

pub(crate) fn deserialize_task_push_notification_config(
    payload: Value,
) -> Result<TaskPushNotificationConfig, A2AError> {
    serde_json::from_value::<TaskPushNotificationConfig>(payload.clone()).or_else(|serde_error| {
        protojson_conv::from_value(payload).map_err(|protojson_error| {
            A2AError::internal(format!(
                "failed to deserialize push-config response: {serde_error}; ProtoJSON fallback failed: {protojson_error}"
            ))
        })
    })
}

pub(crate) fn deserialize_list_task_push_notification_configs_response(
    payload: Value,
) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
    serde_json::from_value::<ListTaskPushNotificationConfigsResponse>(payload.clone()).or_else(
        |serde_error| {
            serde_json::from_value::<Vec<TaskPushNotificationConfig>>(payload.clone())
                .map(|configs| ListTaskPushNotificationConfigsResponse {
                    configs,
                    next_page_token: None,
                })
                .or_else(|array_error| {
                    protojson_conv::from_value(payload).map_err(|protojson_error| {
                        A2AError::internal(format!(
                            "failed to deserialize push-config list response: {serde_error}; array fallback failed: {array_error}; ProtoJSON fallback failed: {protojson_error}"
                        ))
                    })
                })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task_push_config() -> TaskPushNotificationConfig {
        TaskPushNotificationConfig {
            task_id: "t1".into(),
            config: PushNotificationConfig {
                url: "https://example.invalid/webhook".into(),
                id: Some("cfg1".into()),
                token: Some("token-1".into()),
                authentication: Some(AuthenticationInfo {
                    scheme: "Bearer".into(),
                    credentials: Some("secret".into()),
                }),
            },
            tenant: Some("tenant-1".into()),
        }
    }

    #[test]
    fn parses_nested_task_push_config_shape() {
        let payload = serde_json::to_value(sample_task_push_config()).unwrap();
        let parsed = deserialize_task_push_notification_config(payload).unwrap();
        assert_eq!(parsed, sample_task_push_config());
    }

    #[test]
    fn falls_back_to_flattened_protojson_task_push_config_shape() {
        let payload = protojson_conv::to_value(&sample_task_push_config()).unwrap();
        let parsed = deserialize_task_push_notification_config(payload).unwrap();
        assert_eq!(parsed, sample_task_push_config());
    }

    #[test]
    fn parses_nested_list_task_push_configs_shape() {
        let response = ListTaskPushNotificationConfigsResponse {
            configs: vec![sample_task_push_config()],
            next_page_token: Some("next".into()),
        };
        let payload = serde_json::to_value(response.clone()).unwrap();
        let parsed = deserialize_list_task_push_notification_configs_response(payload).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn falls_back_to_flattened_protojson_list_task_push_configs_shape() {
        let response = ListTaskPushNotificationConfigsResponse {
            configs: vec![sample_task_push_config()],
            next_page_token: Some("next".into()),
        };
        let payload = protojson_conv::to_value(&response).unwrap();
        let parsed = deserialize_list_task_push_notification_configs_response(payload).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn falls_back_to_raw_array_list_task_push_configs_shape() {
        let configs = vec![sample_task_push_config()];
        let payload = serde_json::to_value(configs.clone()).unwrap();
        let parsed = deserialize_list_task_push_notification_configs_response(payload).unwrap();
        assert_eq!(parsed.configs, configs);
        assert_eq!(parsed.next_page_token, None);
    }
}