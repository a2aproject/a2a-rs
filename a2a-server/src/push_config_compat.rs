// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::A2AError;
use serde::Serialize;
use serde_json::Value;

pub(crate) fn json_value<T: Serialize>(value: &T) -> Result<Value, A2AError> {
    serde_json::to_value(value)
        .map_err(|e| A2AError::internal(format!("failed to serialize JSON payload: {e}")))
}