use super::*;

#[derive(Debug)]
pub(super) enum BoundedLine {
    Line(Vec<u8>),
    TooLong,
}

/// Read one newline-delimited frame without ever retaining more than `max`
/// bytes of untrusted input. An oversized frame is drained through its newline
/// so the next request starts at a clean frame boundary.
pub(super) async fn read_bounded_line<R>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<BoundedLine>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut too_long = false;

    loop {
        let (consumed, ended) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return if too_long {
                    Ok(Some(BoundedLine::TooLong))
                } else if bytes.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(BoundedLine::Line(bytes)))
                };
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let content_len = newline.unwrap_or(available.len());
            if !too_long {
                if content_len > max.saturating_sub(bytes.len()) {
                    too_long = true;
                    bytes.clear();
                } else {
                    let content = available.get(..content_len).ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid MCP frame length",
                        )
                    })?;
                    bytes.extend_from_slice(content);
                }
            }
            (
                newline.map_or(available.len(), |index| index.saturating_add(1)),
                newline.is_some(),
            )
        };
        reader.consume(consumed);

        if ended {
            return Ok(Some(if too_long {
                BoundedLine::TooLong
            } else {
                BoundedLine::Line(bytes)
            }));
        }
    }
}

pub(super) async fn write_frame_tracked<W>(
    writer: &mut W,
    frame: &impl serde::Serialize,
    started: &mut bool,
) -> Result<(), McpError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    let bytes = line.as_bytes();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let remaining = bytes.get(offset..).ok_or_else(|| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid MCP response write offset",
            ))
        })?;
        let written = match writer.write(remaining).await {
            Ok(written) => written,
            Err(error) => {
                // A writer that panics during poll may have changed its
                // destination before panicking. Treat the output as partial
                // and close this connection rather than append a new frame.
                if flashbrowse_core::async_runtime::is_io_poll_panic(&error) {
                    *started = true;
                }
                return Err(McpError::Io(error));
            }
        };
        if written == 0 {
            return Err(McpError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "MCP response writer accepted zero bytes",
            )));
        }
        if written > remaining.len() {
            *started = true;
            return Err(McpError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP response writer reported more bytes than provided",
            )));
        }
        *started = true;
        offset = offset.checked_add(written).ok_or_else(|| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP response write offset overflow",
            ))
        })?;
    }
    writer.flush().await?;
    Ok(())
}
