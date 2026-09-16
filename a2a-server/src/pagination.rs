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
}
