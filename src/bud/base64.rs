//! Standard base64 (RFC 4648, padded), for `PartView::bytes`.
//!
//! Twenty lines rather than a dependency: this crate decodes exactly one
//! field with it, and a whole crate for that would be the larger surface to
//! audit. Line breaks are skipped, as the server's encoder never emits them
//! but a relay might fold a long value.

use serde::{Deserialize, Deserializer, Serializer};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some(u32::from(c - b'A')),
        b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub(crate) fn decode(input: &str) -> Result<Vec<u8>, String> {
    let clean: Vec<u8> = input
        .bytes()
        .filter(|b| *b != b'\r' && *b != b'\n')
        .collect();
    if clean.len() % 4 != 0 {
        return Err(format!(
            "base64 length {} is not a multiple of 4",
            clean.len()
        ));
    }
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    let quads = clean.len() / 4;
    for (q, quad) in clean.chunks(4).enumerate() {
        let pad = quad.iter().rev().take_while(|b| **b == b'=').count();
        if pad > 2 || (pad > 0 && q + 1 != quads) {
            return Err("base64 padding is misplaced".to_string());
        }
        let mut n: u32 = 0;
        for &c in &quad[..4 - pad] {
            let v = value(c).ok_or_else(|| format!("byte {c:#04x} is not base64"))?;
            n = (n << 6) | v;
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Ok(out)
}

pub(crate) fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let mut n: u32 = 0;
        for (i, b) in chunk.iter().enumerate() {
            n |= u32::from(*b) << (16 - 8 * i);
        }
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub(crate) fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&encode(bytes))
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let s = Option::<String>::deserialize(d)?.unwrap_or_default();
    decode(&s).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_padding() {
        for input in [&b""[..], b"h", b"he", b"hel", b"hello", &[0u8, 255, 128, 7]] {
            assert_eq!(decode(&encode(input)).unwrap(), input);
        }
        assert_eq!(decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn refuses_what_is_not_base64() {
        assert!(decode("aGVsbG8").is_err());
        assert!(decode("aG=sbG8=").is_err());
        assert!(decode("aGV*bG8=").is_err());
        assert!(decode("a===").is_err());
    }
}
