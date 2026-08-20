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

    // LZ-string's Base64 transport is itself strict, padded Base64. Preserve
    // it before the generic unwrap below; otherwise the unwrap turns the LZ
    // bitstream into arbitrary bytes and the lz64 route can never see its
    // encoded alphabet. We still sniff a decoded gzip first, so content wins
    // over a lying compression hint.
    let lz_payload = matches!(hint.as_deref(), Some("lz64") | Some("lz-string"))
        .then(|| std::str::from_utf8(&payload).ok().map(str::to_owned))
        .flatten();

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
        if let Some(text) = lz_payload.as_deref() {
            match decompress_lz64_bounded(text, MAX_DECOMPRESSED_BYTES) {
                Ok(decompressed) => return Ok(decompressed),
                Err(DecodeError::TooLarge) => return Err(DecodeError::TooLarge),
                Err(DecodeError::Undecodable) => {}
            }
        }
        // Also accept one outer Base64 wrapper around the LZ transport.
        if let Ok(text) = std::str::from_utf8(&payload)
            && lz_payload.as_deref() != Some(text)
        {
            match decompress_lz64_bounded(text, MAX_DECOMPRESSED_BYTES) {
                Ok(decompressed) => return Ok(decompressed),
                Err(DecodeError::TooLarge) => return Err(DecodeError::TooLarge),
                Err(DecodeError::Undecodable) => {}
            }
        }
        // Fall through: hint lied; the payload may just be JSON.
    }

    String::from_utf8(payload).map_err(|_| DecodeError::Undecodable)
}

/// Decode lz-string's base64 representation without ever materializing an
/// unbounded decompressed buffer. The upstream helper returns a complete
/// `Vec<u16>` before the caller can inspect its size, which defeats the capture
/// edge's decompression budget.
fn decompress_lz64_bounded(payload: &str, maximum_bytes: usize) -> Result<String, DecodeError> {
    const BASE64_KEY: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
    const U8_CODE: u8 = 0;
    const U16_CODE: u8 = 1;
    const CLOSE_CODE: u8 = 2;

    struct Bits<I> {
        value: u16,
        input: I,
        position: u16,
    }

    impl<I: Iterator<Item = u16>> Bits<I> {
        fn new(mut input: I) -> Option<Self> {
            Some(Self {
                value: input.next()?,
                input,
                position: 1 << 5,
            })
        }

        fn read_bit(&mut self) -> Option<bool> {
            let set = self.value & self.position != 0;
            self.position >>= 1;
            if self.position == 0 {
                self.position = 1 << 5;
                self.value = self.input.next()?;
            }
            Some(set)
        }

        fn read_bits(&mut self, count: u8) -> Option<u32> {
            let mut result = 0_u32;
            for bit in 0..count {
                result |= u32::from(self.read_bit()?) << bit;
            }
            Some(result)
        }
    }

    struct Utf16Output {
        text: String,
        pending_high_surrogate: Option<u16>,
        maximum_bytes: usize,
    }

    impl Utf16Output {
        fn append(&mut self, units: &[u16]) -> Result<(), DecodeError> {
            for &unit in units {
                let character = if let Some(high) = self.pending_high_surrogate.take() {
                    if !(0xdc00..=0xdfff).contains(&unit) {
                        return Err(DecodeError::Undecodable);
                    }
                    let scalar =
                        0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00);
                    char::from_u32(scalar).ok_or(DecodeError::Undecodable)?
                } else if (0xd800..=0xdbff).contains(&unit) {
                    self.pending_high_surrogate = Some(unit);
                    continue;
                } else if (0xdc00..=0xdfff).contains(&unit) {
                    return Err(DecodeError::Undecodable);
                } else {
                    char::from_u32(u32::from(unit)).ok_or(DecodeError::Undecodable)?
                };
                if self.text.len().saturating_add(character.len_utf8()) > self.maximum_bytes {
                    return Err(DecodeError::TooLarge);
                }
                self.text.push(character);
            }
            Ok(())
        }

        fn finish(self) -> Result<String, DecodeError> {
            if self.pending_high_surrogate.is_some() {
                Err(DecodeError::Undecodable)
            } else {
                Ok(self.text)
            }
        }
    }

    let encoded = payload.encode_utf16().map(|unit| {
        BASE64_KEY
            .iter()
            .position(|candidate| u16::from(*candidate) == unit)
            .and_then(|index| u16::try_from(index).ok())
            .ok_or(DecodeError::Undecodable)
    });
    let mut bits = Bits::new(encoded.collect::<Result<Vec<_>, _>>()?.into_iter())
        .ok_or(DecodeError::Undecodable)?;
    let mut dictionary = vec![vec![0_u16], vec![1_u16], vec![2_u16]];
    let initial_code = u8::try_from(bits.read_bits(2).ok_or(DecodeError::Undecodable)?)
        .map_err(|_| DecodeError::Undecodable)?;
    let first = match initial_code {
        U8_CODE => u16::try_from(bits.read_bits(8).ok_or(DecodeError::Undecodable)?)
            .map_err(|_| DecodeError::Undecodable)?,
        U16_CODE => u16::try_from(bits.read_bits(16).ok_or(DecodeError::Undecodable)?)
            .map_err(|_| DecodeError::Undecodable)?,
        CLOSE_CODE => return Ok(String::new()),
        _ => return Err(DecodeError::Undecodable),
    };
    dictionary.push(vec![first]);
    let mut previous = vec![first];
    let mut output = Utf16Output {
        text: String::new(),
        pending_high_surrogate: None,
        maximum_bytes,
    };
    output.append(&previous)?;
    let mut code_width = 3_u8;
    let mut enlarge_in = 4_u64;

    loop {
        let mut code = bits.read_bits(code_width).ok_or(DecodeError::Undecodable)?;
        match u8::try_from(code) {
            Ok(literal @ (U8_CODE | U16_CODE)) => {
                let width = literal.saturating_mul(8).saturating_add(8);
                let unit = u16::try_from(bits.read_bits(width).ok_or(DecodeError::Undecodable)?)
                    .map_err(|_| DecodeError::Undecodable)?;
                dictionary.push(vec![unit]);
                code = u32::try_from(dictionary.len() - 1).map_err(|_| DecodeError::TooLarge)?;
                enlarge_in = enlarge_in.checked_sub(1).ok_or(DecodeError::Undecodable)?;
            }
            Ok(CLOSE_CODE) => return output.finish(),
            _ => {}
        }

        if enlarge_in == 0 {
            enlarge_in = 1_u64
                .checked_shl(u32::from(code_width))
                .ok_or(DecodeError::TooLarge)?;
            code_width = code_width.checked_add(1).ok_or(DecodeError::TooLarge)?;
        }

        let index = usize::try_from(code).map_err(|_| DecodeError::Undecodable)?;
        let entry = if let Some(known) = dictionary.get(index) {
            known.clone()
        } else if index == dictionary.len() {
            let mut next = previous.clone();
            next.push(*previous.first().ok_or(DecodeError::Undecodable)?);
            next
        } else {
            return Err(DecodeError::Undecodable);
        };
        output.append(&entry)?;

        let mut next = previous;
        next.push(*entry.first().ok_or(DecodeError::Undecodable)?);
        dictionary.push(next);
        enlarge_in = enlarge_in.checked_sub(1).ok_or(DecodeError::Undecodable)?;
        previous = entry;

        if enlarge_in == 0 {
            enlarge_in = 1_u64
                .checked_shl(u32::from(code_width))
                .ok_or(DecodeError::TooLarge)?;
            code_width = code_width.checked_add(1).ok_or(DecodeError::TooLarge)?;
        }
    }
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
            let hex =
                std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| DecodeError::Undecodable)?;
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
    fn lz64_output_is_stopped_at_the_decoded_byte_budget() {
        let input = "x".repeat(1025);
        let compressed = lz_str::compress_to_base64(&input);
        assert_eq!(
            decompress_lz64_bounded(&compressed, 1024),
            Err(DecodeError::TooLarge)
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
