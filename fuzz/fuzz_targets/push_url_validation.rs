// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! `a2a_server::push::sender::validate_push_url` -- the string-level SSRF
//! guard (#124, hardened by #223). See #238 target 4.
//!
//! This covers what the guard is today: a check on the URL string, not on
//! the address it resolves to. #224 tracks the gap that leaves open (a
//! hostname's DNS can point anywhere the string check cannot see) and asks
//! for a connect-time guard on the *resolved* address, reusing
//! `is_blocked_ip` rather than a second copy of it. That is a separate,
//! larger change; fuzzing the guard that exists today does not need to
//! wait for it, and is designed to compose with it once it lands -- this
//! target's oracle already reuses `is_blocked_ip` as ground truth, so a
//! connect-time guard built on the same predicate inherits the same cover.
//!
//! Oracle: `is_blocked_ip` is independent ground truth, extracted from
//! `validate_push_url` rather than re-derived here, so the two cannot
//! drift apart the way a hand-copied predicate could. Any URL whose host
//! parses to a literal IP address in a blocked range must be rejected by
//! `validate_push_url`, regardless of how that address was spelled --
//! decimal, hex, octal, short-form, bracketed, upper-cased, or padded with
//! a trailing dot. #223 was exactly this shape of bug, found by hand on
//! one spelling; this explores the whole space cheaply instead.

#![no_main]

use a2a_server::fuzz_support::{is_blocked_ip, validate_push_url};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // `reqwest::Url` is a bare re-export of `url::Url` (`pub use url::Url`),
    // so this is the identical parser `validate_push_url` calls internally,
    // without pulling reqwest's HTTP/TLS stack into this binary just to
    // parse a string.
    let Ok(url) = url::Url::parse(text) else {
        return;
    };
    let Some(host) = url.host_str() else {
        return;
    };

    let unbracketed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let Ok(ip) = unbracketed.parse::<std::net::IpAddr>() else {
        return;
    };

    if is_blocked_ip(ip) {
        assert!(
            validate_push_url(text).is_err(),
            "host {host:?} (parsed as {ip}, which is_blocked_ip rejects) was allowed through \
             validate_push_url for input {text:?}"
        );
    }
});
