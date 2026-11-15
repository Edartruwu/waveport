//! Where the calls of this command are kept.
//!
//! In a file, unless the command line says otherwise: a call whose
//! process died, or whose machine did, is gone on with from there. Next
//! to the file is another that holds the token of the call that was last
//! begun, which is what `--resume last` reads. A token is the right to
//! take a call over, so that file is its owner's alone where the system
//! has such a thing, and nothing prints a token.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use waveport_ports::journal::{Journal, SessionId};
use waveport_store_journal::{Config, LocalJournal, OpenError, ResumeToken};
use waveport_store_memory::MemoryJournal;

use crate::args::{Agent, JOURNAL, Resume};
use crate::halt::Halt;
use crate::{Context, Failure};

/// A journal in a file, and the file next to it.
struct InFile {
    journal: Arc<LocalJournal>,
    /// Where the journal is.
    path: PathBuf,
    /// The file that holds the token of the call that was last begun.
    last: PathBuf,
}

/// Where calls are kept.
pub(crate) struct Kept {
    journal: Arc<dyn Journal>,
    file: Option<InFile>,
}

/// The file that holds the token of the call last begun with the journal
/// at `journal`: the journal's name with `last` for what follows its
/// last dot.
fn last_of(journal: &Path) -> PathBuf {
    journal.with_extension("last")
}

/// Makes the folder the usual journal is in, its owner's alone where the
/// system has such a thing.
fn make_folder(folder: &Path) -> std::io::Result<()> {
    if folder.as_os_str().is_empty() || folder.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(folder)
}

/// Writes `text` to the file at `path`, in place of what was there, so
/// that the file holds all of the one or all of the other. A file that
/// is made is its owner's alone where the system has such a thing.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let beside = path.with_extension("last-being-written");
    let _ = std::fs::remove_file(&beside);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&beside)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&beside, path)
}

impl Kept {
    /// Opens what `agent` says calls are kept in. Returns it, and what
    /// whoever ran the command should know about it.
    ///
    /// A file that cannot be read is left as it is: the command stops,
    /// and says which file it was.
    pub(crate) fn open(agent: &Agent, halt: &Halt) -> Result<(Self, Vec<String>), Failure> {
        if agent.no_journal {
            return Ok((Self::in_memory(), Vec::new()));
        }
        let named = agent.journal.is_some();
        let path = agent
            .journal
            .clone()
            .unwrap_or_else(|| PathBuf::from(JOURNAL));
        if !named && let Some(folder) = path.parent() {
            make_folder(folder).with(|| format!("{} cannot be made", folder.display()))?;
        }
        let config = Config {
            probe: halt.probe(),
            ..Config::default()
        };
        match LocalJournal::open(&path, config) {
            Ok(journal) => {
                let journal = Arc::new(journal);
                let kept = Self {
                    journal: Arc::clone(&journal) as Arc<dyn Journal>,
                    file: Some(InFile {
                        journal,
                        last: last_of(&path),
                        path,
                    }),
                };
                Ok((kept, Vec::new()))
            }
            // Two calls at once from one folder are both taken. The
            // second is told what it does without.
            Err(OpenError::InUse) if !named => {
                let note = format!(
                    "another waveport has {} open: this one keeps its calls in memory, and one \
                     that breaks off is gone (--journal names another file)",
                    path.display()
                );
                Ok((Self::in_memory(), vec![note]))
            }
            Err(error) => Err(Failure(format!(
                "the calls kept in {} cannot be opened: {error}. The file was left as it is: \
                 --journal names another, and --no-journal keeps calls in memory",
                path.display()
            ))),
        }
    }

    fn in_memory() -> Self {
        Self {
            journal: Arc::new(MemoryJournal::new()),
            file: None,
        }
    }

    /// The journal, for a session to write to.
    pub(crate) fn journal(&self) -> Arc<dyn Journal> {
        Arc::clone(&self.journal)
    }

    /// The token that lets the call `id` be gone on with, if calls are
    /// kept where that can be done.
    pub(crate) async fn issue(&self, id: SessionId) -> Result<Option<ResumeToken>, Failure> {
        let Some(file) = &self.file else {
            return Ok(None);
        };
        let token = file
            .journal
            .issue(id)
            .await
            .with(|| format!("{} cannot be written to", file.path.display()))?;
        Ok(Some(token))
    }

    /// Makes `token` the one that `--resume last` finds.
    pub(crate) fn remember(&self, token: &ResumeToken) -> Result<(), Failure> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        write_private(&file.last, &format!("{token}\n"))
            .with(|| format!("{} cannot be written", file.last.display()))
    }

    /// The token that `from` names.
    pub(crate) fn token(&self, from: &Resume) -> Result<ResumeToken, Failure> {
        let Some(file) = &self.file else {
            return Err(Failure(
                "calls are kept in memory here, and none can be gone on with".to_owned(),
            ));
        };
        let holds = match from {
            Resume::Last => &file.last,
            Resume::File(named) => named,
        };
        let text = match std::fs::read_to_string(holds) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && *from == Resume::Last => {
                return Err(Failure(format!(
                    "there is no call to go on with: none was begun with {}, or the last one \
                     ended",
                    file.path.display()
                )));
            }
            Err(error) => {
                return Err(Failure(format!(
                    "{} cannot be read: {error}",
                    holds.display()
                )));
            }
        };
        text.parse()
            .with(|| format!("{} holds something else", holds.display()))
    }

    /// The call that `token` lets its holder go on with, if the journal
    /// has such a call.
    pub(crate) async fn admit(&self, token: &ResumeToken) -> Result<Option<SessionId>, Failure> {
        let Some(file) = &self.file else {
            return Ok(None);
        };
        let admitted = file
            .journal
            .admit(token)
            .await
            .with(|| format!("{} cannot be read", file.path.display()))?;
        Ok(admitted.then(|| token.session()))
    }

    /// The call to go on with, as `from` names it.
    pub(crate) async fn resume(&self, from: &Resume) -> Result<SessionId, Failure> {
        let token = self.token(from)?;
        self.admit(&token).await?.ok_or_else(|| {
            let journal = self
                .file
                .as_ref()
                .map_or_else(String::new, |file| file.path.display().to_string());
            Failure(format!(
                "the call is not in {journal}: it ended, or it is kept in another file"
            ))
        })
    }

    /// What whoever runs the command is told of the calls that broke off
    /// earlier and are still kept, if there are any: nothing takes them
    /// out of the file but going on with them to their end.
    pub(crate) async fn earlier(&self) -> Option<String> {
        let file = self.file.as_ref()?;
        let count = file.journal.sessions().await.ok()?.len();
        let (calls, are) = match count {
            0 => return None,
            1 => ("call", "is"),
            _ => ("calls", "are"),
        };
        Some(format!(
            "{count} {calls} that broke off earlier {are} kept in {} (deleting the file lets go \
             of every call in it)",
            file.path.display()
        ))
    }

    /// Whether a call that broke off here can be gone on with.
    pub(crate) const fn outlasts_the_command(&self) -> bool {
        self.file.is_some()
    }

    /// Lets go of the call `id`, which is over: nobody goes on with it.
    pub(crate) async fn forget(&self, id: SessionId) {
        let Some(file) = &self.file else {
            return;
        };
        if let Err(error) = file.journal.forget(id).await {
            eprintln!(
                "waveport: the call could not be taken out of {}: {error}",
                file.path.display()
            );
        }
        // The call that was last begun is no longer one to go on with.
        let last = std::fs::read_to_string(&file.last)
            .ok()
            .and_then(|text| text.parse::<ResumeToken>().ok());
        if last.is_some_and(|token| token.session() == id) {
            let _ = std::fs::remove_file(&file.last);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use clap::Parser;
    use waveport_ports::journal::SessionId;

    use super::{Kept, last_of, write_private};
    use crate::args::{Agent, Cli, Command, Resume};
    use crate::halt::Halt;

    /// A folder for what a test writes, empty.
    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir()
            .join(format!("waveport-cli-{}", std::process::id()))
            .join(name);
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn agent(arguments: &[&str]) -> Agent {
        let all = ["waveport", "dev"].iter().chain(arguments).copied();
        match Cli::try_parse_from(all).unwrap().command {
            Command::Dev(dev) => dev.agent,
            _ => panic!("dev is dev"),
        }
    }

    fn kept_in(journal: &Path) -> Kept {
        let agent = agent(&["--journal", journal.to_str().unwrap()]);
        let (kept, notes) = Kept::open(&agent, &Halt::Nowhere).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        kept
    }

    #[test]
    fn the_token_of_the_last_call_is_next_to_the_journal() {
        assert_eq!(
            last_of(Path::new(".waveport/journal.redb")),
            Path::new(".waveport/journal.last")
        );
        assert_eq!(last_of(Path::new("calls")), Path::new("calls.last"));
    }

    #[tokio::test]
    async fn a_call_that_was_begun_is_one_to_go_on_with_until_it_is_over() {
        let folder = folder("begun");
        let journal = folder.join("calls.redb");
        let kept = kept_in(&journal);
        assert!(kept.outlasts_the_command());
        // Nothing was begun yet.
        assert_eq!(kept.earlier().await, None);
        let Err(none) = kept.resume(&Resume::Last).await else {
            panic!("nothing was begun");
        };
        assert!(
            none.0.contains("there is no call to go on with"),
            "{}",
            none.0
        );

        let id = SessionId(0xCA11);
        let token = kept.issue(id).await.unwrap().unwrap();
        kept.remember(&token).unwrap();
        assert_eq!(kept.resume(&Resume::Last).await.unwrap(), id);
        // The file holds the token and a line end, and nothing else.
        let last = folder.join("calls.last");
        assert_eq!(
            std::fs::read_to_string(&last).unwrap(),
            format!("{token}\n")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&last).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }

        // The next command finds the call, by `last` and by a file that
        // was put somewhere else.
        drop(kept);
        let elsewhere = folder.join("monday.token");
        std::fs::copy(&last, &elsewhere).unwrap();
        let kept = kept_in(&journal);
        let told = kept.earlier().await.unwrap();
        assert!(
            told.starts_with("1 call that broke off earlier is kept in "),
            "{told}"
        );
        assert_eq!(kept.resume(&Resume::Last).await.unwrap(), id);
        assert_eq!(
            kept.resume(&Resume::File(elsewhere.clone())).await.unwrap(),
            id
        );

        // A newer call takes its place as the last, and both are there.
        let newer = SessionId(0xCA12);
        let newer_token = kept.issue(newer).await.unwrap().unwrap();
        kept.remember(&newer_token).unwrap();
        assert_eq!(kept.resume(&Resume::Last).await.unwrap(), newer);
        assert_eq!(
            kept.resume(&Resume::File(elsewhere.clone())).await.unwrap(),
            id
        );

        // A call that is over is not one to go on with, and the last
        // call is no longer named once it is over.
        kept.forget(id).await;
        let Err(over) = kept.resume(&Resume::File(elsewhere)).await else {
            panic!("the call is over");
        };
        assert!(over.0.contains("it ended"), "{}", over.0);
        assert!(last.exists());
        kept.forget(newer).await;
        assert!(!last.exists());
        assert_eq!(kept.earlier().await, None);
    }

    #[tokio::test]
    async fn what_is_not_a_token_is_not_gone_on_with() {
        let folder = folder("not-a-token");
        let kept = kept_in(&folder.join("calls.redb"));
        let file = folder.join("calls.last");
        write_private(&file, "wpr1_not-a-token\n").unwrap();
        let Err(failure) = kept.resume(&Resume::Last).await else {
            panic!("it is not a token");
        };
        assert!(failure.0.contains("holds something else"), "{}", failure.0);
        // What is written over is all gone.
        write_private(&file, "x\n").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x\n");
        let Err(failure) = kept.resume(&Resume::File(folder.join("nowhere"))).await else {
            panic!("there is no such file");
        };
        assert!(failure.0.contains("cannot be read"), "{}", failure.0);
    }

    #[tokio::test]
    async fn calls_in_memory_are_not_gone_on_with() {
        let (kept, notes) = Kept::open(&agent(&["--no-journal"]), &Halt::Nowhere).unwrap();
        assert!(notes.is_empty() && !kept.outlasts_the_command());
        assert_eq!(kept.earlier().await, None);
        assert_eq!(kept.issue(SessionId(1)).await.unwrap(), None);
        let Err(failure) = kept.resume(&Resume::Last).await else {
            panic!("nothing is kept");
        };
        assert!(failure.0.contains("kept in memory"), "{}", failure.0);
        kept.forget(SessionId(1)).await;
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_read_is_left_as_it_is() {
        let folder = folder("unreadable");
        let journal = folder.join("calls.redb");
        let text = "Not a journal: a letter that was saved under the wrong name.\n".repeat(200);
        std::fs::write(&journal, &text).unwrap();
        let agent = agent(&["--journal", journal.to_str().unwrap()]);
        let Err(failure) = Kept::open(&agent, &Halt::Nowhere).map(|_| ()) else {
            panic!("it is not a journal");
        };
        assert!(failure.0.contains("was left as it is"), "{}", failure.0);
        assert!(failure.0.contains("calls.redb"), "{}", failure.0);
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), text);

        // A file that another command has open, when it was named.
        let journal = folder.join("busy.redb");
        let first = kept_in(&journal);
        let Err(failure) = Kept::open(
            &super::tests::agent(&["--journal", journal.to_str().unwrap()]),
            &Halt::Nowhere,
        )
        .map(|_| ()) else {
            panic!("the file is in use");
        };
        assert!(failure.0.contains("cannot be opened"), "{}", failure.0);
        drop(first);
    }
}
