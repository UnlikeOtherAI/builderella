//! Standard base64 (RFC 4648 §4, with padding) for log chunk payloads.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `bytes` as padded standard base64.
pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[usize::from(b0 >> 2)]);
        out.push(ALPHABET[usize::from(((b0 & 0x03) << 4) | (b1 >> 4))]);
        if chunk.len() > 1 {
            out.push(ALPHABET[usize::from(((b1 & 0x0f) << 2) | (b2 >> 6))]);
        } else {
            out.push(b'=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[usize::from(b2 & 0x3f)]);
        } else {
            out.push(b'=');
        }
    }
    // Only ASCII bytes were pushed.
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::encode;

    #[test]
    fn matches_the_rfc_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in vectors {
            assert_eq!(encode(plain.as_bytes()), encoded);
        }
        assert_eq!(encode(&[0xff, 0xfe, 0xfd]), "//79");
    }
}
