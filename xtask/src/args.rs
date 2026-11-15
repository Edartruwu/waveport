//! A small command-line parser, so xtask needs no argument-parsing crate.

use crate::error::{Error, Result};

/// The arguments left to parse for one command.
pub(crate) struct Args {
    items: Vec<String>,
}

impl Args {
    pub(crate) fn new(items: &[String]) -> Self {
        Self {
            items: items.to_vec(),
        }
    }

    /// Removes `name` if present and reports whether it was.
    pub(crate) fn flag(&mut self, name: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item != name);
        self.items.len() != before
    }

    /// Removes `name value` or `name=value` and returns the value.
    pub(crate) fn value(&mut self, name: &str) -> Result<Option<String>> {
        let prefix = format!("{name}=");
        let Some(position) = self
            .items
            .iter()
            .position(|item| item == name || item.starts_with(&prefix))
        else {
            return Ok(None);
        };
        let item = self.items.remove(position);
        if let Some(value) = item.strip_prefix(&prefix) {
            return Ok(Some(value.to_owned()));
        }
        if position < self.items.len() {
            Ok(Some(self.items.remove(position)))
        } else {
            Err(Error::new(format!("`{name}` needs a value")))
        }
    }

    /// Returns the positional arguments that are left, rejecting unknown options.
    pub(crate) fn finish(self) -> Result<Vec<String>> {
        if let Some(unknown) = self.items.iter().find(|item| item.starts_with("--")) {
            return Err(Error::new(format!("unknown option `{unknown}`")));
        }
        Ok(self.items)
    }
}

#[cfg(test)]
mod tests {
    use super::Args;

    fn args(items: &[&str]) -> Args {
        Args::new(
            &items
                .iter()
                .map(|item| (*item).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn flag_is_removed_and_reported() {
        let mut parsed = args(&["--strict", "left"]);
        assert!(parsed.flag("--strict"));
        assert!(!parsed.flag("--strict"));
        assert_eq!(parsed.finish().unwrap(), ["left"]);
    }

    #[test]
    fn value_accepts_both_spellings() {
        let mut spaced = args(&["--only", "fmt"]);
        assert_eq!(spaced.value("--only").unwrap().as_deref(), Some("fmt"));
        let mut joined = args(&["--only=fmt"]);
        assert_eq!(joined.value("--only").unwrap().as_deref(), Some("fmt"));
    }

    #[test]
    fn value_without_argument_is_an_error() {
        assert!(args(&["--only"]).value("--only").is_err());
    }

    #[test]
    fn unknown_option_is_rejected() {
        assert!(args(&["--nope"]).finish().is_err());
    }
}
