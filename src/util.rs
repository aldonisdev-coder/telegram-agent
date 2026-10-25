use sha2::Digest;
use teloxide::types::{ChatId, Recipient};

pub fn to_chat_id(s: &str) -> Recipient {
    let s = s.trim();
    if let Ok(id) = s.parse::<i64>() {
        Recipient::Id(ChatId(id))
    } else {
        let name = s.trim_start_matches('@');
        Recipient::ChannelUsername(if s.starts_with('@') {
            s.to_string()
        } else {
            format!("@{}", name)
        })
    }
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key = key.to_vec();
    if key.len() > BLOCK {
        let h = sha2::Sha256::digest(&key);
        key = h.to_vec();
    }
    if key.len() < BLOCK {
        key.resize(BLOCK, 0);
    }
    let mut ipad = vec![0u8; BLOCK];
    let mut opad = vec![0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = key[i] ^ 0x36;
        opad[i] = key[i] ^ 0x5c;
    }
    ipad.extend_from_slice(msg);
    let inner = sha2::Sha256::digest(&ipad);
    opad.extend_from_slice(&inner);
    let outer = sha2::Sha256::digest(&opad);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer);
    out
}
pub fn gen_token(len: usize) -> String {
    // CSPRNG-backed: tokens must be unguessable (sessions, API keys).
    // On OS entropy failure there is no safe fallback -- treat it as an
    // unrecoverable startup-class failure.
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf).expect("OS entropy unavailable");
    let mut out = String::with_capacity(len);
    for b in buf {
        out.push(CHARS[(b as usize) % CHARS.len()] as char);
    }
    out
}

/// Constant-time string equality: XOR-folds over the bytes of both strings
/// with no early exit, so the comparison time does not leak where the first
/// mismatch occurs. Used for webhook signature checks. The padding byte for
/// the shorter input is 0x01 (not 0x00) so a NUL-containing string can never
/// compare equal to a shorter one; length itself is public, so the explicit
/// length check is not a timing leak.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let n = a.len().max(b.len());
    let mut diff: u8 = 0;
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(1);
        let y = b.get(i).copied().unwrap_or(1);
        diff |= x ^ y;
    }
    if a.len() != b.len() {
        diff |= 1;
    }
    diff == 0
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Any requested length yields exactly that many ASCII-alnum chars.
        #[test]
        fn token_shape(len in 0usize..256) {
            let t = gen_token(len);
            prop_assert_eq!(t.len(), len);
            prop_assert!(t.bytes().all(|b| b.is_ascii_alphanumeric()));
        }

        /// Two draws must differ (collision odds at 59^16+ are nil).
        #[test]
        fn token_two_draws_differ(len in 16usize..64) {
            prop_assert_ne!(gen_token(len), gen_token(len));
        }

        /// HMAC is deterministic and flips on any single input-bit change.
        #[test]
        fn hmac_deterministic_and_sensitive(
            key in proptest::collection::vec(any::<u8>(), 0..80),
            msg in proptest::collection::vec(any::<u8>(), 0..200),
            bit in 0usize..2048,
        ) {
            let a = hmac_sha256(&key, &msg);
            prop_assert_eq!(a, hmac_sha256(&key, &msg));
            let mut m2 = msg.clone();
            if m2.is_empty() {
                m2.push(1);
            } else {
                let byte = bit / 8 % m2.len();
                m2[byte] ^= 1 << (bit % 8);
            }
            prop_assert_ne!(a, hmac_sha256(&key, &m2));
            prop_assert_ne!(a, [0u8; 32]);
        }

        /// ct_eq: reflexivity always, and distinct strings never equal.
        #[test]
        fn ct_eq_matches_equality(a in ".*", b in ".*") {
            prop_assert!(ct_eq(&a, &a));
            if a != b {
                prop_assert!(!ct_eq(&a, &b));
            }
        }
    }

    /// RFC 4231 test case 1 for HMAC-SHA256 (official vector).
    #[test]
    fn hmac_rfc4231_case1() {
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            mac.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    /// RFC 4231 test case 2 (key = "Jefe", data = the what-do-ya-know line).
    #[test]
    fn hmac_rfc4231_case2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            mac.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::*;

    #[test]
    fn test_gen_token() {
        let t = gen_token(48);
        assert_eq!(t.len(), 48);
        const CHARS: &str = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
        for c in t.chars() {
            assert!(CHARS.contains(c), "invalid char: {}", c);
        }
        // different lengths
        assert_eq!(gen_token(0).len(), 0);
        assert_eq!(gen_token(10).len(), 10);
    }

    #[test]
    fn test_gen_token_charset() {
        let t = gen_token(128);
        const CHARS: &str = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
        for c in t.chars() {
            assert!(CHARS.contains(c), "invalid char: {}", c);
        }
    }

    #[test]
    fn test_gen_token_unique() {
        let a = gen_token(32);
        let b = gen_token(32);
        assert_ne!(a, b);
    }

    #[test]
    fn test_to_chat_id() {
        use teloxide::types::{ChatId, Recipient};
        // numeric
        assert_eq!(to_chat_id("  12345 "), Recipient::Id(ChatId(12345)));
        // @username
        assert_eq!(
            to_chat_id("@mychannel"),
            Recipient::ChannelUsername("@mychannel".to_string())
        );
    }
}
