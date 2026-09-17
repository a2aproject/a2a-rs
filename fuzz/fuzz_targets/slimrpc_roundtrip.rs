// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! `a2a_slimrpc::common::{encode_proto_message, decode_proto_response}`
//! round trip -- raw protobuf bytes off the message bus, a different
//! transport and a different wire encoding from `protojson_roundtrip`'s
//! JSON text, but the same shape of untrusted-bytes surface. See #238
//! target 6.
//!
//! Mirrors `protojson_roundtrip` rather than inventing a second scheme:
//! the same `arbitrary`-generated native types, reusing
//! `ProtoJsonPayload::{to_proto, try_from_proto}` for the native/proto
//! conversion and swapping the middle encoding for the wire bytes this
//! crate actually sends. The oracle is the same idempotence property for
//! the same reason -- proto3 scalar fields have no wire presence, so
//! identity with the `Arbitrary`-generated value is the wrong check, but a
//! second round trip must change nothing a first one did not already fix.

#![no_main]

use a2a::{
    AgentCard, CancelTaskRequest, GetTaskRequest, ListTasksRequest, SendMessageRequest, Task,
    TaskPushNotificationConfig,
};
use a2a_pb::protojson_conv::ProtoJsonPayload;
use a2a_slimrpc::fuzz_support::{decode_proto_response, encode_proto_message};
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
enum FuzzValue {
    SendMessageRequest(SendMessageRequest),
    GetTaskRequest(GetTaskRequest),
    ListTasksRequest(ListTasksRequest),
    CancelTaskRequest(CancelTaskRequest),
    Task(Task),
    TaskPushNotificationConfig(TaskPushNotificationConfig),
    AgentCard(AgentCard),
}

fn check_idempotent<T>(value: T)
where
    T: ProtoJsonPayload + PartialEq + std::fmt::Debug,
{
    let type_name = std::any::type_name::<T>();
    let proto1 = T::to_proto(&value);
    let bytes1 = encode_proto_message(&proto1);

    let Ok(decoded1) = decode_proto_response::<T::Proto>(bytes1, type_name) else {
        return;
    };
    let Ok(round1) = T::try_from_proto(&decoded1) else {
        return;
    };

    let proto2 = T::to_proto(&round1);
    let bytes2 = encode_proto_message(&proto2);
    let decoded2 = decode_proto_response::<T::Proto>(bytes2, type_name)
        .unwrap_or_else(|e| panic!("re-decoding this crate's own re-encoding failed: {e}"));
    let round2 = T::try_from_proto(&decoded2)
        .unwrap_or_else(|e| panic!("re-converting this crate's own re-decoding failed: {e}"));

    assert_eq!(
        round1, round2,
        "slimrpc's proto round trip has no fixed point for this value"
    );
}

fuzz_target!(|value: FuzzValue| {
    match value {
        FuzzValue::SendMessageRequest(v) => check_idempotent(v),
        FuzzValue::GetTaskRequest(v) => check_idempotent(v),
        FuzzValue::ListTasksRequest(v) => check_idempotent(v),
        FuzzValue::CancelTaskRequest(v) => check_idempotent(v),
        FuzzValue::Task(v) => check_idempotent(v),
        FuzzValue::TaskPushNotificationConfig(v) => check_idempotent(v),
        FuzzValue::AgentCard(v) => check_idempotent(v),
    }
});
