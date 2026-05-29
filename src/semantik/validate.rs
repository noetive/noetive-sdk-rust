use std::collections::HashMap;

use crate::semantik::error::Error;
use crate::semantik::limits::{
    MAX_METADATA_KEYS, MAX_METADATA_KEY_LEN, MAX_METADATA_TOTAL_BYTES, MAX_METADATA_VALUE_LEN,
    MAX_TEXT_BYTES, MAX_VECTOR_DIM,
};

/// Non-empty check on an API key. The SDK does not inspect key format
/// beyond rejecting empty / whitespace-only inputs — the server is the
/// source of truth for key validity, and locking the SDK to a specific
/// prefix shape would break the moment Noetive introduces a new key
/// family.
pub(crate) fn api_key_non_empty(k: &str) -> bool {
    !k.trim().is_empty()
}

/// Reports whether `s` contains any ASCII control character
/// (`0x00`-`0x1F` or `0x7F`). Metadata acceptance rule on the wire.
pub(crate) fn has_control_char(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7F)
}

/// Enforce the three targeting fields every publish/search/subscribe
/// request must carry: a non-empty `namespace`, a non-empty `model`, and
/// an in-range `dimensions`.
///
/// The SDK does not default these. Routing a request to a namespace the
/// caller never named — silently falling back to a shared one — risks
/// publishing sensitive data into a space it was not meant for, so an
/// unset field is a fail-fast preflight error rather than a convenience
/// default. `model` and `dimensions` are likewise model-coupled
/// properties with no server default.
pub(crate) fn validate_target(namespace: &str, model: &str, dimensions: u16) -> Result<(), Error> {
    if namespace.is_empty() {
        return Err(Error::preflight("namespace must not be empty"));
    }
    if model.is_empty() {
        return Err(Error::preflight("model must not be empty"));
    }
    validate_dimensions(dimensions)
}

/// Enforce `1 <= dim <= MAX_VECTOR_DIM`.
pub(crate) fn validate_dimensions(dim: u16) -> Result<(), Error> {
    if dim == 0 {
        return Err(Error::preflight("dimensions must be greater than 0"));
    }
    if dim > MAX_VECTOR_DIM {
        return Err(Error::preflight(format!(
            "dimensions {dim} exceeds maximum {MAX_VECTOR_DIM}"
        )));
    }
    Ok(())
}

/// Enforce metadata constraints matching the server: at most
/// [`MAX_METADATA_KEYS`] entries, each key <= [`MAX_METADATA_KEY_LEN`],
/// each value <= [`MAX_METADATA_VALUE_LEN`], total key+value bytes <=
/// [`MAX_METADATA_TOTAL_BYTES`], and no control characters in keys or
/// values.
pub(crate) fn validate_metadata(md: &HashMap<String, String>) -> Result<(), Error> {
    if md.is_empty() {
        return Ok(());
    }
    if md.len() > MAX_METADATA_KEYS {
        return Err(Error::preflight(format!(
            "metadata has {} keys, maximum {MAX_METADATA_KEYS}",
            md.len()
        )));
    }
    let mut total = 0usize;
    for (k, v) in md {
        if k.is_empty() {
            return Err(Error::preflight("metadata key must not be empty"));
        }
        // &str is guaranteed UTF-8, so the "valid UTF-8" check Go does
        // is a no-op here.
        if k.len() > MAX_METADATA_KEY_LEN {
            return Err(Error::preflight(format!(
                "metadata key {k:?} exceeds {MAX_METADATA_KEY_LEN} bytes"
            )));
        }
        if v.len() > MAX_METADATA_VALUE_LEN {
            return Err(Error::preflight(format!(
                "metadata value for key {k:?} exceeds {MAX_METADATA_VALUE_LEN} bytes"
            )));
        }
        if has_control_char(k) {
            return Err(Error::preflight(format!(
                "metadata key {k:?} contains control characters"
            )));
        }
        if has_control_char(v) {
            return Err(Error::preflight(format!(
                "metadata value for key {k:?} contains control characters"
            )));
        }
        total += k.len() + v.len();
    }
    if total > MAX_METADATA_TOTAL_BYTES {
        return Err(Error::preflight(format!(
            "metadata total size {total} exceeds {MAX_METADATA_TOTAL_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Enforce "at least one of text or vector", text byte cap, vector dim
/// cap, and vector numeric sanity (no NaN / ±Inf, which would break
/// server-side distance math). Text and vector are allowed together —
/// the server accepts both and uses the vector verbatim while keeping
/// the text alongside for retrieval.
pub(crate) fn validate_publish_item(text: &str, vector: &[f32]) -> Result<(), Error> {
    let has_text = !text.is_empty();
    let has_vec = !vector.is_empty();
    if !has_text && !has_vec {
        return Err(Error::preflight(
            "publish item must have at least one of text or vector",
        ));
    }
    if has_text && text.len() > MAX_TEXT_BYTES {
        return Err(Error::preflight(format!(
            "publish text exceeds {MAX_TEXT_BYTES} bytes"
        )));
    }
    if has_vec {
        if vector.len() > MAX_VECTOR_DIM as usize {
            return Err(Error::preflight(format!(
                "publish vector dim {} exceeds maximum {MAX_VECTOR_DIM}",
                vector.len()
            )));
        }
        for (i, f) in vector.iter().enumerate() {
            if !f.is_finite() {
                return Err(Error::preflight(format!(
                    "publish vector index {i} is NaN or Inf"
                )));
            }
        }
    }
    Ok(())
}

/// Enforce that a publish item's vector length matches the request's
/// `dimensions` field. The server rejects a mismatch with
/// `invalid_request`; failing fast here avoids burning a round-trip.
pub(crate) fn validate_vector_matches_dimensions(
    vector: &[f32],
    dimensions: u16,
) -> Result<(), Error> {
    if vector.is_empty() {
        return Ok(());
    }
    if vector.len() != dimensions as usize {
        return Err(Error::preflight(format!(
            "publish vector dim {} does not match request dimensions {}",
            vector.len(),
            dimensions
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn api_key_non_empty_accepts_any_non_blank() {
        assert!(api_key_non_empty("keyu_abc"));
        assert!(api_key_non_empty("keyt_abc"));
        assert!(api_key_non_empty("some_new_format_key"));
        assert!(api_key_non_empty("sk-foo"));
        assert!(!api_key_non_empty(""));
        assert!(!api_key_non_empty("   "));
        assert!(!api_key_non_empty("\t\n"));
    }

    #[test]
    fn dimensions_bounds() {
        assert!(validate_dimensions(0).is_err());
        assert!(validate_dimensions(1).is_ok());
        assert!(validate_dimensions(MAX_VECTOR_DIM).is_ok());
        assert!(validate_dimensions(MAX_VECTOR_DIM + 1).is_err());
    }

    #[test]
    fn target_requires_all_three_fields() {
        // All present → ok.
        assert!(validate_target("global", "Qwen3-Embedding-4B", 1024).is_ok());
        // Each missing field is rejected — no silent default.
        assert!(validate_target("", "m", 1024).is_err());
        assert!(validate_target("global", "", 1024).is_err());
        assert!(validate_target("global", "m", 0).is_err());
    }

    #[test]
    fn metadata_empty_is_ok() {
        let m: HashMap<String, String> = HashMap::new();
        assert!(validate_metadata(&m).is_ok());
    }

    #[test]
    fn metadata_too_many_keys() {
        let mut m = HashMap::new();
        for i in 0..(MAX_METADATA_KEYS + 1) {
            m.insert(format!("k{i}"), "v".to_string());
        }
        assert!(validate_metadata(&m).is_err());
    }

    #[test]
    fn metadata_key_too_long() {
        let m = md(&[(&"k".repeat(MAX_METADATA_KEY_LEN + 1), "v")]);
        assert!(validate_metadata(&m).is_err());
    }

    #[test]
    fn metadata_value_too_long() {
        let m = md(&[("k", &"v".repeat(MAX_METADATA_VALUE_LEN + 1))]);
        assert!(validate_metadata(&m).is_err());
    }

    #[test]
    fn metadata_control_char_rejected() {
        let m = md(&[("k\x01", "v")]);
        assert!(validate_metadata(&m).is_err());
        let m = md(&[("k", "v\x7f")]);
        assert!(validate_metadata(&m).is_err());
    }

    #[test]
    fn metadata_total_size_capped() {
        let mut m = HashMap::new();
        // 16 keys, ~256 B value each ≈ 4 KiB total.
        for i in 0..16 {
            m.insert(format!("key{i:02}"), "v".repeat(MAX_METADATA_VALUE_LEN));
        }
        assert!(validate_metadata(&m).is_err());
    }

    #[test]
    fn publish_item_requires_at_least_one() {
        assert!(validate_publish_item("", &[]).is_err());
        // Text + vector together is now allowed: the server keeps the
        // text for retrieval and uses the supplied vector verbatim.
        assert!(validate_publish_item("hello", &[1.0]).is_ok());
        assert!(validate_publish_item("hello", &[]).is_ok());
        assert!(validate_publish_item("", &[1.0]).is_ok());
    }

    #[test]
    fn validate_vector_matches_dimensions_accepts_match() {
        assert!(validate_vector_matches_dimensions(&[1.0, 2.0, 3.0], 3).is_ok());
    }

    #[test]
    fn validate_vector_matches_dimensions_rejects_mismatch() {
        assert!(validate_vector_matches_dimensions(&[1.0, 2.0], 3).is_err());
        assert!(validate_vector_matches_dimensions(&[1.0, 2.0, 3.0, 4.0], 3).is_err());
    }

    #[test]
    fn validate_vector_matches_dimensions_skips_when_empty() {
        // Empty vector means this publish item is text-only; the
        // dimensions check is irrelevant.
        assert!(validate_vector_matches_dimensions(&[], 1024).is_ok());
    }

    #[test]
    fn publish_text_byte_cap() {
        let big = "a".repeat(MAX_TEXT_BYTES + 1);
        assert!(validate_publish_item(&big, &[]).is_err());
        let max = "a".repeat(MAX_TEXT_BYTES);
        assert!(validate_publish_item(&max, &[]).is_ok());
    }

    #[test]
    fn publish_vector_dim_cap() {
        let big = vec![0.0; (MAX_VECTOR_DIM as usize) + 1];
        assert!(validate_publish_item("", &big).is_err());
    }

    #[test]
    fn publish_vector_rejects_nan_inf() {
        assert!(validate_publish_item("", &[1.0, f32::NAN]).is_err());
        assert!(validate_publish_item("", &[1.0, f32::INFINITY]).is_err());
        assert!(validate_publish_item("", &[1.0, f32::NEG_INFINITY]).is_err());
    }
}
