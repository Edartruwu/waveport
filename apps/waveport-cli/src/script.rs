//! The script a caller follows in a scripted call.
//!
//! A text file, one step a line. Lines that are empty or begin with `#`
//! say nothing.
//!
//! | Line | The caller |
//! |---|---|
//! | `silence 500` | says nothing for 500 ms |
//! | `say question.wav` | says what the file holds |
//! | `listen 800 30000` | waits until the agent has spoken and then been silent for 800 ms, for 30 s at most |
//!
//! Files are named from where the script is.

use core::time::Duration;
use std::path::Path;

use waveport_adapter_kit::wav;
use waveport_transport_local_audio::Step;

/// How long a caller listens at most, when the script does not say.
const LISTEN_AT_MOST: Duration = Duration::from_secs(30);

/// Reads the script in the file at `path`.
///
/// # Errors
///
/// The file cannot be read, or a line of it is not a step, or a file it
/// names is not audio that can be said. The error says which line.
pub(crate) fn read(path: &Path) -> Result<Vec<Step>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("the script {} cannot be read: {error}", path.display()))?;
    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    parse(&text, |name| {
        let file = folder.join(name);
        let bytes = std::fs::read(&file)
            .map_err(|error| format!("{} cannot be read: {error}", file.display()))?;
        let audio = wav::read(&bytes).map_err(|error| format!("{}: {error}", file.display()))?;
        Step::speech(&audio).map_err(|error| format!("{}: {error}", file.display()))
    })
    .map_err(|(line, error)| format!("{}, line {line}: {error}", path.display()))
}

/// Reads a script from its text. `say` makes the step of a file's name.
/// An error comes with the number of its line.
fn parse(
    text: &str,
    mut say: impl FnMut(&str) -> Result<Step, String>,
) -> Result<Vec<Step>, (usize, String)> {
    let millis = |word: Option<&str>, what: &str| -> Result<Duration, String> {
        let word = word.ok_or_else(|| format!("{what} is missing, in milliseconds"))?;
        let millis: u64 = word
            .parse()
            .map_err(|_| format!("{word:?} is not a number of milliseconds"))?;
        Ok(Duration::from_millis(millis))
    };
    let mut steps = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let mut words = rest.split_whitespace();
        let step = match verb {
            "silence" => millis(words.next(), "how long").map(Step::Silence),
            "say" if rest.is_empty() => Err("the file to say is missing".to_owned()),
            "say" => say(rest),
            "listen" => {
                millis(words.next(), "how long the agent has to be silent").and_then(|quiet| {
                    let at_most = match words.next() {
                        Some(word) => millis(Some(word), "how long at most")?,
                        None => LISTEN_AT_MOST,
                    };
                    Ok(Step::Listen { quiet, at_most })
                })
            }
            other => Err(format!(
                "{other:?} is not a step: there are silence, say and listen"
            )),
        };
        steps.push(step.map_err(|error| (n + 1, error))?);
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use waveport_transport_local_audio::Step;

    use super::parse;

    fn said(name: &str) -> Result<Step, String> {
        if name == "missing.wav" {
            return Err("no such file".to_owned());
        }
        Ok(Step::Speech(vec![i16::try_from(name.len()).unwrap()]))
    }

    #[test]
    fn a_script_is_its_steps_in_order() {
        let script = "# A caller with one question.\n\
                      silence 500\n\
                      \n\
                      say a question.wav\n\
                      \tlisten 800\n\
                      listen 200 4000\n";
        assert_eq!(
            parse(script, said).unwrap(),
            [
                Step::Silence(Duration::from_millis(500)),
                Step::Speech(vec![14]),
                Step::Listen {
                    quiet: Duration::from_millis(800),
                    at_most: Duration::from_secs(30),
                },
                Step::Listen {
                    quiet: Duration::from_millis(200),
                    at_most: Duration::from_secs(4),
                },
            ]
        );
        assert_eq!(parse("", said).unwrap(), []);
    }

    #[test]
    fn a_line_that_is_not_a_step_is_named() {
        let wrong = |script: &str| parse(script, said).unwrap_err();
        assert_eq!(wrong("silence 5\nshout hello").0, 2);
        assert!(wrong("shout hello").1.contains("\"shout\" is not a step"));
        assert!(wrong("silence").1.contains("missing"));
        assert!(wrong("silence soon").1.contains("\"soon\""));
        assert!(wrong("say").1.contains("the file to say is missing"));
        assert_eq!(wrong("\n\nsay missing.wav"), (3, "no such file".to_owned()));
        assert!(wrong("listen 100 long").1.contains("\"long\""));
    }
}
