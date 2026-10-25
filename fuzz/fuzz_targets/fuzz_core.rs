#![no_main]
//! Fuzz target for the security-relevant pure functions. One combined target
//! (first byte selects the function) keeps the sanitizer build count at one.
//!
//!   cargo fuzz run fuzz_core            # ASan, release
//!   make fuzz FUZZ_SECS=60              # bounded campaign via the Makefile

use libfuzzer_sys::fuzz_target;

/// Inverse of `esc`. Order matters: `&amp;` must decode LAST — decoding it
/// first would fabricate new entity sequences (e.g. esc("&#39;") is
/// "&amp;#39;"; restoring & early lets the &#39; pass corrupt it).
fn decode_entities(e: &str) -> String {
    e.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fuzz_target!(|data: &[u8]| {
    let Some(&tag) = data.first() else { return };
    let rest = &data[1..];
    match tag % 5 {
        // esc(): any UTF-8 input must produce metachar-free output that
        // exactly round-trips through entity decoding.
        0 => {
            if let Ok(s) = std::str::from_utf8(rest) {
                let e = telegram_agent::web::esc(s);
                for c in ['<', '>', '"', '\''] {
                    assert!(!e.contains(c), "raw {c} survived escaping: {e:?}");
                }
                assert_eq!(decode_entities(&e), s);
            }
        }
        // HMAC-SHA256 + ct_eq: correct digest self-verifies, a mutated digest
        // never verifies, for arbitrary key/message splits of the input.
        1 => {
            if rest.len() >= 3 {
                let third = rest.len() / 3;
                let (k, m) = (&rest[..third], &rest[third..2 * third]);
                let mac = telegram_agent::util::hmac_sha256(k, m);
                let hex: String = mac.iter().map(|b| format!("{:02x}", b)).collect();
                assert_eq!(hex.len(), 64);
                assert!(telegram_agent::util::ct_eq(&hex, &hex));
                let mut bad = hex.clone().into_bytes();
                bad[0] ^= 1;
                let bad = String::from_utf8(bad).expect("hex mutation kept utf8");
                assert!(!telegram_agent::util::ct_eq(&hex, &bad));
                // determinism on recompute
                let again = telegram_agent::util::hmac_sha256(k, m);
                assert_eq!(mac, again);
            }
        }
        // extract_invoice_items: arbitrary bytes parsed as JSON must never
        // panic, in every wrapping shape.
        2 => {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(rest) {
                let _ = telegram_agent::api::extract_invoice_items(&v);
                let _ = telegram_agent::api::extract_invoice_items(&serde_json::json!({
                    "items": v.clone()
                }));
                let _ = telegram_agent::api::extract_invoice_items(&serde_json::json!({
                    "result": { "items": v }
                }));
            }
        }
        // gen_token: any requested length yields exactly that many
        // alphanumeric characters.
        3 => {
            let n = rest.first().copied().unwrap_or(0) as usize;
            let t = telegram_agent::util::gen_token(n);
            assert_eq!(t.len(), n);
            assert!(
                t.bytes().all(|b| b.is_ascii_alphanumeric()),
                "invalid charset: {t}"
            );
        }
        // ct_eq: reflexivity for arbitrary strings, and inequality detected
        // between a string and its arbitrary prefix.
        4 => {
            if let Ok(a) = std::str::from_utf8(rest) {
                let half = &rest[..rest.len() / 2];
                if let Ok(b) = std::str::from_utf8(half) {
                    assert!(telegram_agent::util::ct_eq(a, a));
                    if a != b {
                        assert!(!telegram_agent::util::ct_eq(a, b));
                    }
                }
            }
        }
        _ => unreachable!(),
    }
});
