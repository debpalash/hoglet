//! Payload decoding for capture endpoints (spec/wire-compat.md "Decompression").
//!
//! Order is load-bearing and mirrors PostHog's own capture service:
//! 1. `application/x-www-form-urlencoded` → unwrap `data=<...>` first (the
//!    sendBeacon path), restoring the `+`→space urldecode quirk.
//! 2. Speculative strict-base64 unwrap of the whole payload.
//! 3. Gzip magic-byte sniff → gzip, regardless of the `compression` hint —
//!    clients lie about it.
//! 4. `lz64` hint → lz-string base64 decode (legacy posthog-js only).
//! 5. Raw UTF-8 JSON.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use std::io::Read;

/// Hard ceiling on decompressed size — gzip bombs must fail before allocating
/// anything close to this.
pub const MAX_DECOMPRESSED_BYTES: usize = 64 * 1024 * 1024;

/// Decompression reads in chunks of this size and re-checks the ceiling.
const GZIP_CHUNK_BYTES: usize = 64 * 1024;

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Payload could not be decoded to UTF-8 JSON text by any accepted route.
    Undecodable,
    /// Decompressed size exceeded [`MAX_DECOMPRESSED_BYTES`].
    TooLarge,
}

/// Decode a capture request body to JSON text.
///
/// `form_encoded` is true when Content-Type is
/// `application/x-www-form-urlencoded`; `compression_hint` is the query/form
/// `compression` value, trusted only for the legacy lz64 route.
pub fn decode(
    body: &[u8],
    form_encoded: bool,
    compression_hint: Option<&str>,
) -> Result<String, DecodeError> {
    let mut hint = compression_hint.map(str::to_owned);
    let mut payload: Vec<u8>;

    if form_encoded {
        // Clients lie about content types too: curl defaults to this
        // Content-Type for any --data body. If there's no data= field,
        // treat the body as raw.
        match parse_form(body) {
            Ok((data, form_hint)) => {
                if form_hint.is_some() {
                    hint = form_hint;
                }
                payload = data;
            }
            Err(_) => payload = body.to_vec(),
        }
    } else {
        payload = body.to_vec();
    }

    // Speculative base64 unwrap: strict alphabet, length % 4 == 0.
    if looks_like_base64(&payload)
        && let Ok(decoded) = BASE64.decode(&payload)
    {
        payload = decoded;
    }

    // Content sniffing beats the hint: gzip is gzip no matter what the client
    // claimed.
    if payload.starts_with(&GZIP_MAGIC) {
        let decompressed = gunzip_bounded(&payload)?;
        return String::from_utf8(decompressed).map_err(|_| DecodeError::Undecodable);
    }

    if matches!(hint.as_deref(), Some("lz64") | Some("lz-string")) {
        let text = std::str::from_utf8(&payload).map_err(|_| DecodeError::Undecodable)?;
        if let Some(utf16) = lz_str::decompress_from_base64(text) {
            return String::from_utf16(&utf16).map_err(|_| DecodeError::Undecodable);
        }
        // Fall through: hint lied; the payload may just be JSON.
    }

    String::from_utf8(payload).map_err(|_| DecodeError::Undecodable)
}

/// Parse `data=<json-or-base64>&compression=<hint>`.
///
/// The `+`→space quirk: urldecoding turns `+` into space, but `+` is a valid
/// base64 character. If the decoded value looks like base64-with-spaces,
/// restore spaces to `+` (spec/wire-compat.md).
fn parse_form(body: &[u8]) -> Result<(Vec<u8>, Option<String>), DecodeError> {
    let text = std::str::from_utf8(body).map_err(|_| DecodeError::Undecodable)?;
    let mut data: Option<String> = None;
    let mut hint: Option<String> = None;
    for pair in text.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "data" => {
                let decoded = urlencoding_decode(value)?;
                data = Some(decoded);
            }
            "compression" => hint = Some(value.to_string()),
            _ => {}
        }
    }
    let mut data = data.ok_or(DecodeError::Undecodable)?;
    if data.contains(' ') {
        let restored = data.replace(' ', "+");
        if looks_like_base64(restored.as_bytes()) {
            data = restored;
        }
    }
    Ok((data.into_bytes(), hint))
}

fn urlencoding_decode(value: &str) -> Result<String, DecodeError> {
    let with_spaces = value.replace('+', " ");
    let mut out = Vec::with_capacity(with_spaces.len());
    let bytes = with_spaces.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| DecodeError::Undecodable)?;
            let byte = u8::from_str_radix(hex, 16).map_err(|_| DecodeError::Undecodable)?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| DecodeError::Undecodable)
}

fn looks_like_base64(payload: &[u8]) -> bool {
    !payload.is_empty()
        && payload.len().is_multiple_of(4)
        && payload
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

/// Gzip decompress with a hard output ceiling, reading in bounded chunks so a
/// bomb fails early instead of allocating [`MAX_DECOMPRESSED_BYTES`] up front.
fn gunzip_bounded(payload: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut decoder = flate2::read::GzDecoder::new(payload);
    let mut out = Vec::new();
    let mut chunk = [0u8; GZIP_CHUNK_BYTES];
    loop {
        match decoder.read(&mut chunk) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                if out.len() + n > MAX_DECOMPRESSED_BYTES {
                    return Err(DecodeError::TooLarge);
                }
                out.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return Err(DecodeError::Undecodable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn raw_json_passes_through() {
        assert_eq!(decode(b"{\"a\":1}", false, None).unwrap(), "{\"a\":1}");
    }

    #[test]
    fn gzip_sniffed_even_with_wrong_hint() {
        let body = gzip(b"{\"a\":1}");
        // Hint says lz64; magic bytes say gzip; gzip wins.
        assert_eq!(decode(&body, false, Some("lz64")).unwrap(), "{\"a\":1}");
    }

    #[test]
    fn base64_wrapped_gzip_unwraps() {
        let body = BASE64.encode(gzip(b"{\"a\":1}"));
        assert_eq!(decode(body.as_bytes(), false, None).unwrap(), "{\"a\":1}");
    }

    #[test]
    fn base64_wrapped_json_unwraps() {
        let body = BASE64.encode(b"{\"a\":1}");
        assert_eq!(decode(body.as_bytes(), false, None).unwrap(), "{\"a\":1}");
    }

    #[test]
    fn lz64_with_hint() {
        let compressed = lz_str::compress_to_base64("{\"a\":1}");
        // lz64 output isn't length%4==0-safe for the speculative pass in
        // general, but must decode via the hint route.
        assert_eq!(
            decode(compressed.as_bytes(), false, Some("lz64")).unwrap(),
            "{\"a\":1}"
        );
    }

    #[test]
    fn form_encoded_beacon_path() {
        let json = b"{\"a\":1}";
        let data = BASE64
            .encode(json)
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D");
        let body = format!("data={data}&compression=base64");
        assert_eq!(decode(body.as_bytes(), true, None).unwrap(), "{\"a\":1}");
    }

    #[test]
    fn form_encoded_plus_space_quirk_restored() {
        // base64(">>>") = "Pj4+". The '+' arrives already urldecoded to a
        // space; the parser must restore it and decode successfully.
        let body = "data=Pj4 ";
        assert_eq!(decode(body.as_bytes(), true, None).unwrap(), ">>>");
    }

    #[test]
    fn gzip_bomb_hits_ceiling() {
        let zeros = vec![0u8; MAX_DECOMPRESSED_BYTES + 1];
        let bomb = gzip(&zeros);
        assert_eq!(decode(&bomb, false, None), Err(DecodeError::TooLarge));
    }

    #[test]
    fn garbage_is_undecodable() {
        assert_eq!(
            decode(&[0xff, 0xfe, 0x01], false, None),
            Err(DecodeError::Undecodable)
        );
    }
}
