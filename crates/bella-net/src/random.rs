//! Random bytes from the operating system.

/// The operating system's random number generator failed.
///
/// Secrets are never produced from a fallback generator, so callers must
/// treat this as fatal for the operation that needed the secret.
#[derive(Debug, thiserror::Error)]
#[error("the operating system random number generator failed: {source}")]
pub struct RandomError {
    #[source]
    source: getrandom::Error,
}

/// Fills `buf` from the operating system generator.
pub(crate) fn fill(buf: &mut [u8]) -> Result<(), RandomError> {
    getrandom::fill(buf).map_err(|source| RandomError { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_distinct_buffers() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        fill(&mut a).unwrap();
        fill(&mut b).unwrap();
        assert_ne!(a, [0u8; 32]);
        assert_ne!(a, b);
    }
}
