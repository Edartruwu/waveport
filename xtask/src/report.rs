//! How gates describe and print what they found.

/// One rule broken by one workspace package.
#[derive(Debug)]
pub(crate) struct Violation {
    /// Stable rule identifier, such as `ARCH002`.
    pub(crate) code: &'static str,
    /// One line saying what is wrong.
    pub(crate) summary: String,
    /// The file to fix, relative to the workspace root.
    pub(crate) file: String,
    /// Labelled detail lines: the dependency path, the rule, how to fix it.
    pub(crate) notes: Vec<(&'static str, String)>,
}

impl Violation {
    pub(crate) fn new(code: &'static str, summary: impl Into<String>, file: &str) -> Self {
        Self {
            code,
            summary: summary.into(),
            file: file.to_owned(),
            notes: Vec::new(),
        }
    }

    #[must_use]
    pub(crate) fn note(mut self, label: &'static str, text: impl Into<String>) -> Self {
        self.notes.push((label, text.into()));
        self
    }
}

/// Prints the outcome of one gate over workspace packages and returns
/// whether it passed.
pub(crate) fn finish(gate: &str, violations: &[Violation], checked: usize) -> bool {
    finish_with(gate, violations, checked, "workspace packages")
}

/// Like [`finish`], for a gate that checks something other than packages.
pub(crate) fn finish_with(
    gate: &str,
    violations: &[Violation],
    checked: usize,
    unit: &str,
) -> bool {
    if violations.is_empty() {
        println!("{gate}: ok ({checked} {unit})");
        return true;
    }
    let annotate = std::env::var("GITHUB_ACTIONS").is_ok_and(|value| value == "true");
    for violation in violations {
        eprintln!("error[{}]: {}", violation.code, violation.summary);
        eprintln!("  --> {}", violation.file);
        for (label, text) in &violation.notes {
            eprintln!("   = {label}: {text}");
        }
        eprintln!();
        if annotate {
            println!(
                "::error file={},title={}::{}",
                violation.file, violation.code, violation.summary
            );
        }
    }
    let plural = if violations.len() == 1 {
        "violation"
    } else {
        "violations"
    };
    eprintln!("{gate}: {} {plural} in {checked} {unit}", violations.len());
    false
}
