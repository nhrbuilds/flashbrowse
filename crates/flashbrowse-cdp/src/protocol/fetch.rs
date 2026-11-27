//! `Fetch` domain types for lazy request interception and route mocking.

use serde_json::{json, Value};

use crate::error::CdpError;
use crate::transport::CdpClient;

/// Enable `Fetch.requestPaused` interception for all URLs in `session_id`.
pub async fn enable(client: &CdpClient, session_id: &str) -> Result<(), CdpError> {
    let _: Value = client
        .send(
            Some(session_id),
            "Fetch.enable",
            json!({
                "patterns": [{ "urlPattern": "*" }]
            }),
        )
        .await?;
    Ok(())
}

/// Disable `Fetch` interception in `session_id`.
pub async fn disable(client: &CdpClient, session_id: &str) -> Result<(), CdpError> {
    let _: Value = client
        .send(Some(session_id), "Fetch.disable", json!({}))
        .await?;
    Ok(())
}

/// Fulfill a paused request with a synthetic HTTP status, headers, and body.
pub async fn fulfill_request(
    client: &CdpClient,
    session_id: &str,
    request_id: &str,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<(), CdpError> {
    let response_headers: Vec<Value> = headers
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    let _: Value = client
        .send(
            Some(session_id),
            "Fetch.fulfillRequest",
            json!({
                "requestId": request_id,
                "responseCode": status,
                "responseHeaders": response_headers,
                "body": encode_base64(body),
            }),
        )
        .await?;
    Ok(())
}

/// Continue a paused request unmodified.
pub async fn continue_request(
    client: &CdpClient,
    session_id: &str,
    request_id: &str,
) -> Result<(), CdpError> {
    let _: Value = client
        .send(
            Some(session_id),
            "Fetch.continueRequest",
            json!({ "requestId": request_id }),
        )
        .await?;
    Ok(())
}

/// Abort a paused request with a network error reason (e.g. `"Failed"` or `"BlockedByClient"`).
pub async fn fail_request(
    client: &CdpClient,
    session_id: &str,
    request_id: &str,
    reason: &str,
) -> Result<(), CdpError> {
    let _: Value = client
        .send(
            Some(session_id),
            "Fetch.failRequest",
            json!({
                "requestId": request_id,
                "errorReason": reason,
            }),
        )
        .await?;
    Ok(())
}

/// Standard RFC 4648 base64 encoder (avoids extra crate dependency).
pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let alphabet = |index| char::from(TABLE.get(index).copied().unwrap_or_default());
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or_default());
        let b1 = u32::from(chunk.get(1).copied().unwrap_or_default());
        let b2 = u32::from(chunk.get(2).copied().unwrap_or_default());
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet(((triple >> 18) & 0x3f) as usize));
        out.push(alphabet(((triple >> 12) & 0x3f) as usize));
        if chunk.len() > 1 {
            out.push(alphabet(((triple >> 6) & 0x3f) as usize));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(alphabet((triple & 0x3f) as usize));
        } else {
            out.push('=');
        }
    }
    out
}

/// Standard RFC 4648 base64 decoder.
pub fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let mut vals: Vec<u8> = Vec::with_capacity(input.len());
    let mut pad = 0usize;
    for b in input.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            pad = pad.saturating_add(1);
            if pad > 2 {
                return Err("too many base64 padding bytes".to_string());
            }
            vals.push(0);
            continue;
        }
        if pad > 0 {
            return Err("unexpected data after base64 padding".to_string());
        }
        let v = match b {
            b'A'..=b'Z' => b.saturating_sub(b'A'),
            b'a'..=b'z' => b.saturating_sub(b'a').saturating_add(26),
            b'0'..=b'9' => b.saturating_sub(b'0').saturating_add(52),
            b'+' => 62,
            b'/' => 63,
            _ => return Err(format!("invalid base64 byte: {b}")),
        };
        vals.push(v);
    }
    if !vals.len().is_multiple_of(4) {
        return Err("invalid base64 length".to_string());
    }
    let mut out = Vec::with_capacity(
        vals.len()
            .checked_div(4)
            .unwrap_or_default()
            .saturating_mul(3),
    );
    let mut chunks = vals.as_chunks::<4>().0.iter().peekable();
    while let Some(chunk) = chunks.next() {
        let is_last = chunks.peek().is_none();
        let triple = ((chunk[0] as u32) << 18)
            | ((chunk[1] as u32) << 12)
            | ((chunk[2] as u32) << 6)
            | (chunk[3] as u32);
        out.push(((triple >> 16) & 0xff) as u8);
        if !(is_last && pad >= 2) {
            out.push(((triple >> 8) & 0xff) as u8);
        }
        if !(is_last && pad >= 1) {
            out.push((triple & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{decode_base64, encode_base64};

    #[test]
    fn base64_roundtrip_and_padding_checks() {
        for raw in [b"".as_slice(), b"a", b"ab", b"abc", b"hello world!"] {
            let enc = encode_base64(raw);
            assert_eq!(decode_base64(&enc).unwrap(), raw);
        }
        assert!(decode_base64("YW=j").is_err());
        assert!(decode_base64("A===").is_err());
        assert!(decode_base64("abc").is_err());
    }
}
