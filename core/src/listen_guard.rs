//! What every listening socket in this app agrees on.
//!
//! There are three of them now — the hook endpoint, the OneBot server, and
//! remote access — and they had independently grown the same two rules with
//! different wording, plus one of them comparing a token with a naked `==`.
//! Three copies of a security check is three chances for one of them to be the
//! wrong one, and the wrong one is not discoverable by reading the other two.

/// Refuse to listen beyond loopback without a token worth having.
///
/// Loopback is exempt because reaching it already means running code on the
/// machine. Everything else is the LAN, where the token is the only thing
/// between a conversation history and whoever else is on the wifi — so a short
/// one is refused rather than warned about, at the point of configuration
/// rather than at the point of breach.
///
/// `subject` names the token in the message the user reads ("the hook token",
/// "the OneBot access token").
pub fn validate_listen_config(host: &str, token: Option<&str>, subject: &str) -> Result<(), String> {
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    if is_loopback {
        return Ok(());
    }
    match token {
        Some(t) if t.len() >= MIN_TOKEN_LEN => Ok(()),
        Some(_) => Err(format!(
            "{subject} is too short for a non-loopback address (need at least {MIN_TOKEN_LEN} characters); use a longer one or bind to 127.0.0.1"
        )),
        None => Err(format!(
            "refusing to listen on a non-loopback address without {subject}; set one or bind to 127.0.0.1"
        )),
    }
}

/// Short enough to be typed into a phone once, long enough that guessing it
/// over a LAN is not a plan.
const MIN_TOKEN_LEN: usize = 16;

/// A 32-character hex token. Not a secret anyone memorises, so length beats
/// shape.
pub fn generate_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Comparison that does not return early on the first differing byte.
///
/// Over loopback this guards little, but the alternative is a naked `==` on a
/// secret — which is the kind of thing that stops being local the day someone
/// binds to `0.0.0.0`. Which is exactly what remote access asks them to do.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reaching loopback already means running code on the machine, so a token
    /// there guards nothing that is not already lost.
    #[test]
    fn loopback_needs_no_token() {
        assert!(validate_listen_config("127.0.0.1", None, "the token").is_ok());
        assert!(validate_listen_config("localhost", None, "the token").is_ok());
        assert!(validate_listen_config("::1", None, "the token").is_ok());
    }

    #[test]
    fn a_public_bind_needs_a_long_token() {
        assert!(validate_listen_config("0.0.0.0", None, "the token").is_err());
        assert!(validate_listen_config("0.0.0.0", Some("123456789012345"), "the token").is_err());
        assert!(validate_listen_config("0.0.0.0", Some("1234567890123456"), "the token").is_ok());
    }

    /// The address remote access actually gets bound to. `0.0.0.0` is the
    /// wildcard and a LAN address is what a phone is told to dial, and both have
    /// to be on the far side of the rule.
    #[test]
    fn a_lan_address_is_not_loopback() {
        assert!(validate_listen_config("192.168.1.10", None, "the token").is_err());
        assert!(validate_listen_config("192.168.1.10", Some(&generate_token()), "the token").is_ok());
        // Tailscale hands out 100.64.0.0/10, which is not loopback either.
        assert!(validate_listen_config("100.101.102.103", None, "the token").is_err());
    }

    /// The generated one has to pass the check it exists for.
    #[test]
    fn a_generated_token_is_long_enough_to_bind_publicly() {
        let token = generate_token();
        assert_eq!(token.len(), 32);
        assert!(validate_listen_config("0.0.0.0", Some(&token), "the token").is_ok());
    }

    #[test]
    fn constant_time_eq_still_compares() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
