// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Newly issued stateless cursors are caller-bound. Legacy unsigned positions
//! remain readable for compatibility; visibility is rechecked on every page.

use super::store::TaskListScope;
use a2a::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::sync::OnceLock;

type SortKey = (Option<chrono::DateTime<chrono::Utc>>, String);
const MAX_TOKEN_BYTES: usize = 16_384;

pub(super) enum Position {
    Offset(usize),
    After(SortKey),
}

pub(super) fn listing_query(req: &ListTasksRequest) -> ListTasksRequest {
    let mut query = req.clone();
    query.page_token = None;
    query.page_size = None;
    query.history_length = None;
    query.timeline_length = None;
    query.include_artifacts = None;
    query
}

#[derive(Default)]
pub(super) struct KeysetCursors {
    key: OnceLock<[u8; 32]>,
}

impl KeysetCursors {
    fn mac(
        &self,
        payload: &str,
        req: &ListTasksRequest,
        scope: &TaskListScope,
    ) -> Result<Hmac<Sha256>, A2AError> {
        if self.key.get().is_none() {
            let mut key = [0; 32];
            getrandom::fill(&mut key)
                .map_err(|_| A2AError::internal("cannot initialize listing cursors"))?;
            let _ = self.key.set(key);
        }
        let key = self
            .key
            .get()
            .ok_or_else(|| A2AError::internal("listing cursor key is unavailable"))?;
        let mut mac = Hmac::<Sha256>::new_from_slice(key)
            .map_err(|_| A2AError::internal("cannot authenticate listing cursor"))?;
        let binding = serde_json::to_vec(&(
            scope.tenant.as_ref(),
            scope.principal.as_ref(),
            listing_query(req),
        ))
        .map_err(|_| A2AError::internal("cannot bind listing cursor"))?;
        mac.update(b"a2a-rs/list/v1\0");
        mac.update(payload.as_bytes());
        mac.update(b"\0");
        mac.update(&binding);
        Ok(mac)
    }

    pub(super) fn encode(
        &self,
        task: &Task,
        req: &ListTasksRequest,
        scope: &TaskListScope,
    ) -> Result<String, A2AError> {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&(task.status.timestamp, &task.id))
                .map_err(|_| A2AError::internal("cannot encode listing position"))?,
        );
        let tag = self.mac(&payload, req, scope)?.finalize().into_bytes();
        let token = format!("keyset:{payload}.{}", URL_SAFE_NO_PAD.encode(tag));
        if token.len() > MAX_TOKEN_BYTES {
            return Err(A2AError::invalid_params(
                "task identifier is too long to paginate",
            ));
        }
        Ok(token)
    }

    pub(super) fn position(
        &self,
        token: Option<&str>,
        req: &ListTasksRequest,
        scope: &TaskListScope,
    ) -> Result<Position, A2AError> {
        let Some(token) = token.filter(|token| !token.is_empty()) else {
            return Ok(Position::Offset(0));
        };
        if token.len() > MAX_TOKEN_BYTES {
            return Err(A2AError::invalid_params("invalid page token"));
        }
        // Legacy tokens are positions, not authentication or scope proofs.
        if token.bytes().all(|byte| byte.is_ascii_digit()) {
            return token
                .parse()
                .map(Position::Offset)
                .map_err(|_| A2AError::invalid_params("invalid page token"));
        }
        let payload = if let Some(signed) = token.strip_prefix("keyset:") {
            let (payload, tag) = signed
                .split_once('.')
                .ok_or_else(|| A2AError::invalid_params("invalid page token"))?;
            let tag = URL_SAFE_NO_PAD
                .decode(tag)
                .map_err(|_| A2AError::invalid_params("invalid page token"))?;
            self.mac(payload, req, scope)?
                .verify_slice(&tag)
                .map_err(|_| A2AError::invalid_params("invalid page token"))?;
            payload
        } else {
            token
        }; // Read legacy sort-key tokens without retaining state.
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| A2AError::invalid_params("invalid page token"))?;
        serde_json::from_slice(&bytes)
            .map(Position::After)
            .map_err(|_| A2AError::invalid_params("invalid page token"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn task() -> Task {
        Task {
            id: "a".into(),
            context_id: "ctx".into(),
            generation: 0,
            timeline: None,
            status: TaskStatus {
                state: TaskState::Working,
                timestamp: None,
                message: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }
    fn request() -> ListTasksRequest {
        serde_json::from_value(serde_json::json!({"pageSize":1})).unwrap()
    }

    #[test]
    fn stateless_cursors_are_bound_to_store_query_and_caller() {
        let cursors = KeysetCursors::default();
        let req = request();
        let scope = TaskListScope {
            tenant: Some("tenant".into()),
            principal: Some((true, "alice".into())),
        };
        let token = cursors.encode(&task(), &req, &scope).unwrap();
        assert!(
            matches!(cursors.position(Some(&token), &req, &scope).unwrap(), Position::After((None, id)) if id == "a")
        );
        let mut projection = req.clone();
        projection.page_size = Some(10);
        projection.history_length = Some(0);
        assert!(cursors.position(Some(&token), &projection, &scope).is_ok());
        assert!(
            KeysetCursors::default()
                .position(Some(&token), &req, &scope)
                .is_err()
        );
        let mut other = scope.clone();
        other.tenant = Some("other".into());
        assert!(cursors.position(Some(&token), &req, &other).is_err());
        other = scope.clone();
        other.principal = Some((true, "bob".into()));
        assert!(cursors.position(Some(&token), &req, &other).is_err());
        let mut query = req.clone();
        query.status = Some(TaskState::Completed);
        assert!(cursors.position(Some(&token), &query, &scope).is_err());
        assert!(
            cursors
                .position(Some(&format!("{token}x")), &req, &scope)
                .is_err()
        );
    }

    #[test]
    fn cursor_parsing_rejects_unbounded_or_malformed_tokens_and_reads_legacy_positions() {
        let cursors = KeysetCursors::default();
        let req = request();
        let scope = TaskListScope::default();
        for token in [
            "x".repeat(MAX_TOKEN_BYTES + 1),
            "keyset:missing-tag".into(),
            "keyset:!.!".into(),
            "keyset:YWJj.ZA".into(),
            "999999999999999999999999999999999999999999".into(),
        ] {
            assert!(cursors.position(Some(&token), &req, &scope).is_err());
        }
        assert!(matches!(
            cursors.position(None, &req, &scope).unwrap(),
            Position::Offset(0)
        ));
        assert!(matches!(
            cursors.position(Some("2"), &req, &scope).unwrap(),
            Position::Offset(2)
        ));
        let mut oversized = task();
        oversized.id = "x".repeat(MAX_TOKEN_BYTES);
        assert!(cursors.encode(&oversized, &req, &scope).is_err());
    }
}
