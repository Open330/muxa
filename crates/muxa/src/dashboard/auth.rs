//! Bearer-token authentication for the dashboard HTTP server.
//!
//! [`check_bearer`] is framework-agnostic — it takes the raw
//! `Authorization` header value (or `None`) and an expected token, and
//! returns whether the request should pass. The axum middleware that
//! wraps it lives in this module's caller in a later commit.
//!
//! Comparison is constant-time and length-padded so an attacker can't
//! infer either the token's length or any prefix-match progress from
//! response timing. Length information is technically not secret for
//! fixed-length tokens (e.g. an `openssl rand`'d 32-byte hex), but the
//! padding is cheap and we get to make this guarantee unconditionally.

use subtle::ConstantTimeEq;

/// Constant-time check: does `header` carry a `Bearer <token>` that
/// matches `expected`? Returns `false` for `None`, missing prefix, or
/// any mismatch.
///
/// If `expected` is empty, the function returns `false` for every
/// possible input — callers should not invoke this at all when no token
/// is configured (the [`DashboardConfig`](super::DashboardConfig) layer
/// already gates auth on `Option<String>`).
#[must_use]
pub fn check_bearer(header: Option<&str>, expected: &str) -> bool {
    let Some(h) = header else {
        return false;
    };
    let Some(got) = h.strip_prefix("Bearer ") else {
        return false;
    };

    // Pad both sides to the longer length with NUL so the byte-wise
    // comparison runs in time independent of the inputs' shapes. We
    // additionally compare lengths in constant time and AND the two
    // results — content-equal-after-pad alone could spuriously match
    // shorter strings that happen to be NUL-suffixed.
    let max = got.len().max(expected.len());
    let mut a = vec![0u8; max];
    let mut b = vec![0u8; max];
    a[..got.len()].copy_from_slice(got.as_bytes());
    b[..expected.len()].copy_from_slice(expected.as_bytes());

    let same_content = a.ct_eq(&b);
    let same_len = (got.len() as u64).ct_eq(&(expected.len() as u64));
    bool::from(same_content & same_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correct_token_returns_true() {
        assert!(check_bearer(Some("Bearer s3cret"), "s3cret"));
    }

    #[test]
    fn wrong_token_returns_false() {
        assert!(!check_bearer(Some("Bearer s3cret"), "differ"));
    }

    #[test]
    fn shorter_got_returns_false() {
        assert!(!check_bearer(Some("Bearer abc"), "abcdef"));
    }

    #[test]
    fn longer_got_returns_false() {
        assert!(!check_bearer(Some("Bearer abcdef"), "abc"));
    }

    #[test]
    fn missing_bearer_prefix_returns_false() {
        assert!(!check_bearer(Some("s3cret"), "s3cret"));
        assert!(!check_bearer(Some("Token s3cret"), "s3cret"));
    }

    #[test]
    fn no_header_returns_false() {
        assert!(!check_bearer(None, "s3cret"));
    }

    #[test]
    fn empty_token_after_bearer_returns_false() {
        assert!(!check_bearer(Some("Bearer "), "s3cret"));
    }

    #[test]
    fn padded_input_does_not_spuriously_match() {
        // got = "abc\0", expected = "abc" — different lengths, must reject.
        assert!(!check_bearer(Some("Bearer abc\0"), "abc"));
    }
}
