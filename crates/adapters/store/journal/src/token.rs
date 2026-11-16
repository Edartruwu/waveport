//! The token that lets a session be come back to.

use core::fmt;
use core::str::FromStr;

use sha2::{Digest as _, Sha256};
use waveport_ports::error::PortError;
use waveport_ports::journal::SessionId;

/// What every token begins with, so that one is known when it is seen,
/// and so that a later kind of token can be told from this one.
const PREFIX: &str = "wpr1_";

/// What is hashed before a token, so that the hash of a token is the
/// hash of nothing else.
const DOMAIN: &[u8] = b"waveport resume token v1\0";

/// The right to go on with a session: its name and 16 bytes that nobody
/// can guess.
///
/// Whoever holds a session's token can take the session over, so it is
/// to be kept as a password is. A journal keeps only its hash: what is
/// read out of a journal's file is not a token. Written out, a token is
/// `wpr1_` and 64 hexadecimal digits.
#[derive(Clone, PartialEq, Eq)]
pub struct ResumeToken {
    session: SessionId,
    secret: [u8; 16],
}

impl ResumeToken {
    /// A new token for `session`, from the operating system's entropy.
    ///
    /// # Errors
    ///
    /// The operating system gave no entropy.
    pub(crate) fn new(session: SessionId) -> Result<Self, PortError> {
        let mut secret = [0; 16];
        getrandom::fill(&mut secret).map_err(|error| {
            PortError::transient(format!("the operating system gave no entropy: {error}"))
        })?;
        Ok(Self { session, secret })
    }

    /// The session the token is for.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// What a journal keeps of the token.
    pub(crate) fn hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(DOMAIN);
        hasher.update(self.session.0.to_be_bytes());
        hasher.update(self.secret);
        hasher.finalize().into()
    }
}

/// Whether two hashes are the same, found out in the same time whether
/// they are or not.
pub(crate) fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0, |differ, (a, b)| differ | (a ^ b)) == 0
}

impl fmt::Display for ResumeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{:032x}", self.session.0)?;
        self.secret
            .iter()
            .try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for ResumeToken {
    /// The session, and not the part that is secret: what is debugged is
    /// logged.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ResumeToken({}, ..)", self.session)
    }
}

/// Why a piece of text is not a resume token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotAToken;

impl fmt::Display for NotAToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "not a resume token: one is `{PREFIX}` and 64 hexadecimal digits"
        )
    }
}

impl core::error::Error for NotAToken {}

impl FromStr for ResumeToken {
    type Err = NotAToken;

    fn from_str(written: &str) -> Result<Self, NotAToken> {
        let digits = written.trim().strip_prefix(PREFIX).ok_or(NotAToken)?;
        // Lower-case digits only: a token is written one way.
        let plain = |digit: u8| digit.is_ascii_digit() || (b'a'..=b'f').contains(&digit);
        if digits.len() != 64 || !digits.bytes().all(plain) {
            return Err(NotAToken);
        }
        let (session, secret) = digits.split_at_checked(32).ok_or(NotAToken)?;
        let session = u128::from_str_radix(session, 16).map_err(|_| NotAToken)?;
        let secret = u128::from_str_radix(secret, 16).map_err(|_| NotAToken)?;
        Ok(Self {
            session: SessionId(session),
            secret: secret.to_be_bytes(),
        })
    }
}

#[cfg(test)]
mod tests {
    use waveport_ports::journal::SessionId;

    use super::{NotAToken, ResumeToken, same};

    #[test]
    fn a_token_is_read_back_from_how_it_is_written() {
        let token = ResumeToken::new(SessionId(0xABC)).unwrap();
        let written = token.to_string();
        assert_eq!(written.len(), 5 + 64);
        assert!(
            written.starts_with("wpr1_00000000000000000000000000000abc"),
            "{written}"
        );
        let back: ResumeToken = written.parse().unwrap();
        assert_eq!(back, token);
        assert_eq!(back.session(), SessionId(0xABC));
        assert_eq!(back.hash(), token.hash());
        // With the white space a file or a terminal puts around it.
        assert_eq!(
            format!("  {written}\n").parse::<ResumeToken>().unwrap(),
            token
        );
    }

    #[test]
    fn two_tokens_for_one_session_are_not_the_same() {
        let session = SessionId(7);
        let one = ResumeToken::new(session).unwrap();
        let other = ResumeToken::new(session).unwrap();
        assert_ne!(one, other);
        assert_ne!(one.hash(), other.hash());
        assert!(same(&one.hash(), &one.hash()));
        assert!(!same(&one.hash(), &other.hash()));
        // The same secret for another session is another token.
        let secret = &one.to_string()[37..];
        let moved: ResumeToken = format!("wpr1_{:032x}{secret}", 8).parse().unwrap();
        assert_ne!(moved.hash(), one.hash());
    }

    #[test]
    fn a_token_does_not_show_in_what_is_logged() {
        let token = ResumeToken::new(SessionId(0x5E55)).unwrap();
        let secret = &token.to_string()[37..];
        let logged = format!("{token:?}");
        assert!(!logged.contains(secret), "{logged}");
        assert!(logged.contains("5e55"), "{logged}");
    }

    #[test]
    fn what_is_not_a_token_is_refused() {
        let good = ResumeToken::new(SessionId(1)).unwrap().to_string();
        let cases = [
            String::new(),
            "wpr1_".to_owned(),
            good[5..].to_owned(),
            good.replacen("wpr1_", "wpr2_", 1),
            good[..good.len() - 1].to_owned(),
            format!("{good}0"),
            good.to_uppercase(),
            good.replacen('0', "g", 1),
            good.replacen('0', "+", 1),
        ];
        for case in cases {
            assert_eq!(case.parse::<ResumeToken>(), Err(NotAToken), "{case}");
        }
    }
}
