//! Keys, kept out of what gets printed.

use core::fmt;

/// A key for a provider.
///
/// It never shows itself: printed for debugging or as text it reads
/// `Secret(…)`. The only way to the value is [`expose`](Self::expose),
/// which is called where the key is put into a request and nowhere else.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Box<str>);

impl Secret {
    /// A key from its value.
    #[must_use]
    pub fn new(value: impl Into<Box<str>>) -> Self {
        Self(value.into())
    }

    /// The key in the environment variable `name`.
    ///
    /// # Errors
    ///
    /// The variable is not set, is empty, or is not text.
    pub fn from_env(name: &str) -> Result<Self, MissingSecret> {
        match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => Ok(Self::new(value.trim())),
            _ => Err(MissingSecret {
                variable: name.into(),
            }),
        }
    }

    /// The value.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `text` with every occurrence of the key struck out. What a provider
    /// sends back, an error page for one, can repeat the key it was given.
    #[must_use]
    pub fn strike(&self, text: &str) -> String {
        if self.0.is_empty() {
            text.to_owned()
        } else {
            text.replace(&*self.0, "…")
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(…)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// The environment variable that should hold a key does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingSecret {
    /// The variable.
    pub variable: Box<str>,
}

impl fmt::Display for MissingSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the environment variable {} is not set", self.variable)
    }
}

impl core::error::Error for MissingSecret {}

#[cfg(test)]
mod tests {
    use super::{MissingSecret, Secret};

    #[test]
    fn a_key_does_not_show_itself() {
        #[derive(Debug)]
        struct Config {
            #[expect(dead_code, reason = "read by the derived Debug only")]
            key: Secret,
        }
        let key = Secret::new("sk-12345");
        assert_eq!(format!("{key:?}"), "Secret(…)");
        assert_eq!(key.to_string(), "Secret(…)");
        assert_eq!(key.expose(), "sk-12345");
        let shown = format!("{:?}", Config { key });
        assert!(!shown.contains("12345"), "{shown}");
    }

    #[test]
    fn a_key_is_struck_out_of_text_that_repeats_it() {
        let key = Secret::new("sk-12345");
        assert_eq!(
            key.strike("Incorrect API key provided: sk-12345. Find yours at…"),
            "Incorrect API key provided: …. Find yours at…"
        );
        assert_eq!(
            Secret::new("").strike("nothing to strike"),
            "nothing to strike"
        );
    }

    #[test]
    fn a_variable_that_is_not_set_is_named() {
        let missing = Secret::from_env("WAVEPORT_TEST_NO_SUCH_VARIABLE").unwrap_err();
        assert_eq!(
            missing,
            MissingSecret {
                variable: "WAVEPORT_TEST_NO_SUCH_VARIABLE".into()
            }
        );
        assert_eq!(
            missing.to_string(),
            "the environment variable WAVEPORT_TEST_NO_SUCH_VARIABLE is not set"
        );
        // PATH is set wherever tests run.
        assert!(Secret::from_env("PATH").is_ok());
    }
}
