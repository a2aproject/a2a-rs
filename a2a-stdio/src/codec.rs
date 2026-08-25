// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use crate::frame::Frame;
use crate::frame::FrameHeaders;
use crate::frame::State;
use bytes::{Buf, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

pub const DEFAULT_MAX_FRAME_SIZE: usize = 1024 * 1024; // 3.7 recommends 1 MiB
pub const DEFAULT_MAX_HEADER_SIZE: usize = 8 * 1024; // not in spec; see note below
pub const HEADER_SIZE_ESTIMATE: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum StdioCodecError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error), // From<io::Error> is REQUIRED by Decoder
    #[error("missing content length")]
    MissingContentLength,
    #[error("invalid content length: {0}")]
    InvalidContentLength(String),
    #[error("frame too large: {len} > {max}")]
    FrameTooLarge { len: usize, max: usize },
    #[error("header block too large: {max}")]
    HeaderBlockTooLarge { max: usize },
    #[error("malformed header: {0}")]
    MalformedHeader(String),
    #[error("unexpected end of input")]
    UnexpectedEndOfInput,
}

pub struct StdioCodec {
    state: State,
    scanned: usize, // how far we've already searched for \r\n\r\n
    max_frame_size: usize,
    max_header_size: usize,
}

impl StdioCodec {
    pub fn new(max_frame_size: usize, max_header_size: usize) -> Self {
        Self {
            state: State::Headers,
            scanned: 0,
            max_frame_size,
            max_header_size,
        }
    }
}

impl Decoder for StdioCodec {
    type Item = Frame;
    type Error = StdioCodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, Self::Error> {
        loop {
            match &mut self.state {
                State::Headers => {
                    let Some(i) = find_separator(src, self.scanned) else {
                        if src.len() > self.max_header_size {
                            return Err(StdioCodecError::HeaderBlockTooLarge {
                                max: self.max_header_size,
                            });
                        }
                        self.scanned = src.len().saturating_sub(3);
                        return Ok(None);
                    };
                    let (content_length, headers) = parse_headers(&src[..i])?;
                    if content_length > self.max_frame_size {
                        return Err(StdioCodecError::FrameTooLarge {
                            len: content_length,
                            max: self.max_frame_size,
                        });
                    }
                    src.advance(i + 4);
                    self.scanned = 0;
                    self.state = State::Body {
                        content_length,
                        headers,
                    };
                }
                State::Body {
                    content_length,
                    headers,
                } => {
                    let content_length = *content_length;
                    if src.len() < content_length {
                        src.reserve(content_length - src.len());
                        return Ok(None);
                    }
                    let headers = std::mem::take(headers);
                    let body = src.split_to(content_length).freeze();
                    self.state = State::Headers;
                    return Ok(Some(Frame { headers, body }));
                }
            }
        }
    }
}

impl Encoder<Frame> for StdioCodec {
    type Error = StdioCodecError;

    fn encode(&mut self, item: Frame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let n = item.body.len();
        if n > self.max_frame_size {
            return Err(StdioCodecError::FrameTooLarge {
                len: n,
                max: self.max_frame_size,
            });
        }

        // Reject CRLF in any peer-influenced header text before writing a single
        // byte: a stray \r\n would inject headers or split the frame, and `dst`
        // is a shared buffer with no rollback once written.
        let h = &item.headers;
        for text in [
            h.content_type.as_deref(),
            h.a2a_kind.as_deref(),
            h.a2a_id.as_deref(),
            h.a2a_method.as_deref(),
        ]
        .into_iter()
        .flatten()
        .chain(h.service_params.keys().map(String::as_str))
        .chain(h.service_params.values().flatten().map(String::as_str))
        {
            if text.contains(['\r', '\n']) {
                return Err(StdioCodecError::MalformedHeader(text.to_owned()));
            }
        }

        dst.reserve(HEADER_SIZE_ESTIMATE + 2 + n);

        // Everything from here is append-only, so `truncate(header_start)` is an
        // exact rollback if the header block turns out to be over the limit.
        let header_start = dst.len();
        put_header(dst, "Content-Length", &n.to_string());
        if let Some(v) = h.content_type.as_deref() {
            put_header(dst, "Content-Type", v);
        }
        if let Some(v) = h.a2a_kind.as_deref() {
            put_header(dst, "A2A-Kind", v);
        }
        if let Some(v) = h.a2a_id.as_deref() {
            put_header(dst, "A2A-Id", v);
        }
        if let Some(v) = h.a2a_method.as_deref() {
            put_header(dst, "A2A-Method", v);
        }
        // One `A2A-SP-<name>: <value>` line per value (15.2).
        for (key, values) in &h.service_params {
            for value in values {
                put_header(dst, &format!("A2A-SP-{key}"), value);
            }
        }
        dst.extend_from_slice(b"\r\n");

        if dst.len() - header_start > self.max_header_size {
            dst.truncate(header_start);
            return Err(StdioCodecError::HeaderBlockTooLarge {
                max: self.max_header_size,
            });
        }

        dst.extend_from_slice(&item.body);

        Ok(())
    }
}

/// Parse a frame's header block (3.1, 15.2), returning `Content-Length` and
/// the recognised headers.
///
/// Header names are compared case-insensitively per RFC 7230. Unrecognised
/// headers are ignored. `Content-Length` is the only required header.
fn parse_headers(block: &[u8]) -> Result<(usize, FrameHeaders), StdioCodecError> {
    let block = std::str::from_utf8(block)
        .map_err(|e| StdioCodecError::MalformedHeader(format!("not valid UTF-8: {e}")))?;

    let mut content_length: Option<usize> = None;
    let mut headers = FrameHeaders::default();

    for line in block.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| StdioCodecError::MalformedHeader(line.to_owned()))?;
        // RFC 7230 forbids whitespace between the field name and the colon, and
        // a leading-whitespace line is deprecated obs-fold. Tolerating either
        // creates parser disagreements, so reject rather than trim.
        if name.trim() != name {
            return Err(StdioCodecError::MalformedHeader(line.to_owned()));
        }
        let value = value.trim();

        if name.eq_ignore_ascii_case("content-length") {
            let len = value
                .parse::<usize>()
                .map_err(|_| StdioCodecError::InvalidContentLength(value.to_owned()))?;
            content_length = Some(len);
        } else if name.eq_ignore_ascii_case("content-type") {
            headers.content_type = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("a2a-kind") {
            headers.a2a_kind = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("a2a-id") {
            headers.a2a_id = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("a2a-method") {
            headers.a2a_method = Some(value.to_owned());
        } else if let Some(key) = strip_prefix_ignore_ascii_case(name, "a2a-sp-") {
            headers
                .service_params
                .entry(key.to_ascii_lowercase())
                .or_default()
                .push(value.to_owned());
        }
    }

    let content_length = content_length.ok_or(StdioCodecError::MissingContentLength)?;
    Ok((content_length, headers))
}

/// Case-insensitive [`str::strip_prefix`]. Returns `None` if `prefix` does not
/// match or does not end on a character boundary.
fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let (head, rest) = s.split_at_checked(prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then_some(rest)
}

/// Locate the `\r\n\r\n` header/body separator (3.1), resuming at `from`.
///
/// Returns the offset of the separator's first byte, which is also the length
/// of the header block. `from` lets the caller skip bytes already searched in a
/// previous poll; it is clamped so a separator straddling two reads is not missed.
fn find_separator(src: &[u8], from: usize) -> Option<usize> {
    const SEP: &[u8] = b"\r\n\r\n";
    let last_start = src.len().checked_sub(SEP.len())?;
    let start = from.min(last_start);
    src[start..]
        .windows(SEP.len())
        .position(|w| w == SEP)
        .map(|i| start + i)
}

fn put_header(dst: &mut BytesMut, name: &str, value: &str) {
    dst.extend_from_slice(name.as_bytes());
    dst.extend_from_slice(b": ");
    dst.extend_from_slice(value.as_bytes());
    dst.extend_from_slice(b"\r\n");
}
