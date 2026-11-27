// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Shared paging limits for the list endpoints.
//!
//! `TaskStore` and `PushConfigStore` are public traits, so the bound on a
//! response cannot live only in the implementations this crate ships. The
//! handler applies these limits before a store is consulted, and the
//! in-tree stores apply them again — a store reached by another route
//! should still not return an unbounded page.
//!
//! Third-party store implementations are encouraged to use
//! [`resolve_page_size`] so every deployment agrees on what a page is.

use a2a::A2AError;

/// Page size used when the caller does not ask for one.
pub const DEFAULT_PAGE_SIZE: usize = 50;

/// Largest page any list endpoint will return, whatever the caller asks for.
///
/// An oversized request is clamped rather than rejected: the response carries
/// `MAX_PAGE_SIZE` entries and a continuation token for the rest.
pub const MAX_PAGE_SIZE: usize = 100;

/// Resolve a caller's requested page size into the size actually served.
///
/// Absent, zero and negative values fall back to [`DEFAULT_PAGE_SIZE`]; the
/// result never exceeds [`MAX_PAGE_SIZE`].
pub fn resolve_page_size(requested: Option<i32>) -> usize {
    match requested {
        Some(size) if size > 0 => (size as usize).min(MAX_PAGE_SIZE),
        _ => DEFAULT_PAGE_SIZE,
    }
}

/// Resolve a caller's page token into the offset of the first entry to serve.
///
/// An absent token and an empty one both mean the first page. The empty
/// string is what a list response carries in `nextPageToken` when there is no
/// next page, and a client may send it back as it received it. A token that
/// is not a non-negative integer is rejected with `INVALID_PARAMS`.
///
/// Third-party store implementations are encouraged to use this alongside
/// [`resolve_page_size`].
pub fn resolve_page_offset(token: Option<&str>) -> Result<usize, A2AError> {
    match token {
        None | Some("") => Ok(0),
        Some(token) => token
            .parse::<usize>()
            .map_err(|_| A2AError::invalid_params("invalid page token")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_absent_and_non_positive_use_the_default() {
        assert_eq!(resolve_page_size(None), DEFAULT_PAGE_SIZE);
        assert_eq!(resolve_page_size(Some(0)), DEFAULT_PAGE_SIZE);
        assert_eq!(resolve_page_size(Some(-1)), DEFAULT_PAGE_SIZE);
        assert_eq!(resolve_page_size(Some(i32::MIN)), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn test_requested_size_is_honoured_up_to_the_maximum() {
        assert_eq!(resolve_page_size(Some(1)), 1);
        assert_eq!(resolve_page_size(Some(MAX_PAGE_SIZE as i32)), MAX_PAGE_SIZE);
    }

    #[test]
    fn test_oversized_requests_are_clamped_not_rejected() {
        assert_eq!(
            resolve_page_size(Some(MAX_PAGE_SIZE as i32 + 1)),
            MAX_PAGE_SIZE
        );
        assert_eq!(resolve_page_size(Some(1000)), MAX_PAGE_SIZE);
        assert_eq!(resolve_page_size(Some(i32::MAX)), MAX_PAGE_SIZE);
    }

    #[test]
    fn test_absent_and_empty_page_tokens_mean_the_first_page() {
        assert_eq!(resolve_page_offset(None).unwrap(), 0);
        assert_eq!(resolve_page_offset(Some("")).unwrap(), 0);
        assert_eq!(resolve_page_offset(Some("0")).unwrap(), 0);
    }

    #[test]
    fn test_numeric_page_token_is_the_offset() {
        assert_eq!(resolve_page_offset(Some("7")).unwrap(), 7);
    }

    #[test]
    fn test_non_numeric_page_token_is_invalid_params() {
        for token in ["nope", " ", "-1", "1.5"] {
            let err = resolve_page_offset(Some(token)).unwrap_err();
            assert_eq!(err.code, a2a::error_code::INVALID_PARAMS, "{token:?}");
            assert_eq!(err.message, "invalid page token", "{token:?}");
        }
    }
}
