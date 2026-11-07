//! Pairing short-code. Both sides must display and confirm the same code.

/// First six bytes of `blake3(sorted(pub_a, pub_b) || nonce)`.
pub fn sas_code(public_a: &[u8], public_b: &[u8], nonce: &[u8]) -> [u8; 6] {
    let (first, second) = if public_a <= public_b {
        (public_a, public_b)
    } else {
        (public_b, public_a)
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(first);
    hasher.update(second);
    hasher.update(nonce);
    let hash = hasher.finalize();
    let mut code = [0_u8; 6];
    code.copy_from_slice(&hash.as_bytes()[..6]);
    code
}

/// Six decimal digits for display. Derived from the same six hash bytes so a
/// confirmed code cannot be replayed with a different nonce.
pub fn sas_digits(public_a: &[u8], public_b: &[u8], nonce: &[u8]) -> String {
    let code = sas_code(public_a, public_b, nonce);
    let value = u64::from(code[0]) << 40
        | u64::from(code[1]) << 32
        | u64::from(code[2]) << 24
        | u64::from(code[3]) << 16
        | u64::from(code[4]) << 8
        | u64::from(code[5]);
    format!("{:06}", value % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_order_independent() {
        let a = [1_u8; 32];
        let b = [2_u8; 32];
        let nonce = [9_u8; 16];
        assert_eq!(sas_digits(&a, &b, &nonce), sas_digits(&b, &a, &nonce));
        assert_eq!(sas_code(&a, &b, &nonce), sas_code(&b, &a, &nonce));
    }

    #[test]
    fn different_nonce_changes_the_code() {
        let a = [1_u8; 32];
        let b = [2_u8; 32];
        assert_ne!(
            sas_digits(&a, &b, &[9_u8; 16]),
            sas_digits(&a, &b, &[10_u8; 16])
        );
    }

    #[test]
    fn display_is_six_digits() {
        let code = sas_digits(&[1; 32], &[2; 32], &[3; 16]);
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }
}
