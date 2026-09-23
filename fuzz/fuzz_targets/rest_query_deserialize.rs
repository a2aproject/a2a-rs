// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! REST query-string deserialization, reachable pre-auth on every `GET`.
//! See #238 target 5.
//!
//! `serde_urlencoded::from_bytes` is called directly on the raw bytes
//! rather than driving a live router: it is exactly the mechanism
//! `axum::extract::Query<T>` uses internally
//! (`serde_urlencoded::Deserializer::new(form_urlencoded::parse(bytes))`
//! then `T::deserialize`), so this exercises the same parse without the
//! cost of a Tokio runtime and a real HTTP request per input.
//!
//! Oracle: no panic on any of the three query types, and -- the specific
//! line #238 names -- no panic converting a successfully parsed `status`
//! string through `TaskState` via `serde_json::from_value`, the extra step
//! `ListTasksQuery` alone does not cover.

#![no_main]

use a2a::TaskState;
use a2a_server::rest::{GetTaskQuery, ListPushConfigsQuery, ListTasksQuery};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = serde_urlencoded::from_bytes::<GetTaskQuery>(data);
    let _ = serde_urlencoded::from_bytes::<ListPushConfigsQuery>(data);

    if let Ok(query) = serde_urlencoded::from_bytes::<ListTasksQuery>(data) {
        if let Some(status) = query.status {
            let _ = serde_json::from_value::<TaskState>(serde_json::Value::String(status));
        }
    }
});
