// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! LSP-style Content-Length framing for the STDIO transport.
//!
//! Every message on the wire is prefixed with HTTP-style headers:
//!
//! ```text
//! Content-Length: <N>\r\n
//! Content-Type: application/json\r\n
//! \r\n
//! <N bytes of JSON>
//! ```
//!
//! `Content-Type` is optional on read (always written for clarity).

use crate::errors::StdioError;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const CONTENT_LENGTH_PREFIX: &str = "Content-Length: ";

/// Maximum size (in bytes) accepted for a single framed message body.
///
/// Bounds memory allocated from a single peer-supplied `Content-Length` header
/// so a buggy or malicious subprocess cannot trigger a huge allocation by
/// advertising an unrealistic body size.
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024; // 64 MiB

/// Read a single framed message from the given reader.
///
/// Returns the raw JSON bytes of the message body.
/// Returns `Ok(None)` when the reader reaches EOF (pipe closed).
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, StdioError> {
    let mut content_length: Option<usize> = None;

    // Parse headers until we hit the empty \r\n separator line.
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            // EOF — pipe closed.
            return Ok(None);
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);

        // Empty line = end of headers.
        if trimmed.is_empty() {
            break;
        }

        if let Some(value) = trimmed.strip_prefix(CONTENT_LENGTH_PREFIX) {
            content_length = Some(value.parse::<usize>().map_err(|_| {
                StdioError::InvalidHeader(format!("invalid Content-Length value: {value}"))
            })?);
        }
        // Ignore unknown headers (e.g. Content-Type) for forward compatibility.
    }

    let length = content_length
        .ok_or_else(|| StdioError::InvalidHeader("missing Content-Length header".to_string()))?;

    if length > MAX_FRAME_SIZE {
        return Err(StdioError::InvalidHeader(format!(
            "Content-Length {length} exceeds maximum frame size {MAX_FRAME_SIZE}"
        )));
    }

    // Read exactly `length` bytes of body.
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;

    Ok(Some(body))
}

/// Write a single framed message to the given writer.
///
/// Prepends `Content-Length` and `Content-Type` headers, then flushes.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> Result<(), StdioError> {
    let header = format!(
        "Content-Length: {}\r\nContent-Type: application/json\r\n\r\n",
        body.len()
    );
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn test_write_then_read_roundtrip() {
        let message = b"{\"id\":\"1\",\"method\":\"message/send\"}";

        // Write a frame into a buffer.
        let mut buf = Vec::new();
        write_frame(&mut buf, message).await.unwrap();

        // Read it back.
        let mut reader = BufReader::new(Cursor::new(buf));
        let result = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(result, message);
    }

    #[tokio::test]
    async fn test_read_eof_returns_none() {
        let mut reader = BufReader::new(Cursor::new(Vec::<u8>::new()));
        let result = read_frame(&mut reader).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_read_missing_content_length() {
        let raw = b"Content-Type: application/json\r\n\r\n{}";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(matches!(err, StdioError::InvalidHeader(_)));
    }

    #[tokio::test]
    async fn test_read_invalid_content_length() {
        let raw = b"Content-Length: abc\r\n\r\n";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(matches!(err, StdioError::InvalidHeader(_)));
    }

    #[tokio::test]
    async fn test_multiple_frames() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"first").await.unwrap();
        write_frame(&mut buf, b"second").await.unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let first = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(first, b"first");
        let second = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(second, b"second");

        // Third read should be EOF.
        let eof = read_frame(&mut reader).await.unwrap();
        assert!(eof.is_none());
    }

    #[tokio::test]
    async fn test_ignores_unknown_headers() {
        let raw = b"Content-Length: 2\r\nX-Custom: foo\r\nContent-Type: application/json\r\n\r\nhi";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let result = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(result, b"hi");
    }

    #[tokio::test]
    async fn test_rejects_oversized_content_length() {
        let too_big = MAX_FRAME_SIZE + 1;
        let raw = format!("Content-Length: {too_big}\r\n\r\n");
        let mut reader = BufReader::new(Cursor::new(raw.into_bytes()));
        let err = read_frame(&mut reader).await.unwrap_err();
        match err {
            StdioError::InvalidHeader(msg) => {
                assert!(msg.contains("exceeds maximum frame size"), "got: {msg}");
            }
            other => panic!("expected InvalidHeader, got {other:?}"),
        }
    }
}
