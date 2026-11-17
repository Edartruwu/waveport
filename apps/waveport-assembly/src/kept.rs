//! Where the sessions of a process are kept.
//!
//! In a file, unless it is told otherwise: a session whose process died,
//! or whose machine did, is gone on with from there. Next to the file is
//! another that holds the token of the session that was last begun,
//! which is what [`Resume::Last`] reads. A token is the right to take a
//! session over, so that file is its owner's alone where the system has
//! such a thing, and nothing prints a token.

use core::time::Duration;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use waveport_ports::journal::{Journal, SessionId};
use waveport_store_journal::{Config, LocalJournal, OpenError, ResumeToken};
use waveport_store_memory::MemoryJournal;

use crate::halt::Halt;
use crate::spec::{JOURNAL, Keeps, Named};
use crate::{Context, Failure};

/// The session to go on with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resume {
    /// The one that was last begun with the journal.
    Last,
    /// The one whose token this file holds.
    File(PathBuf),
}

impl core::str::FromStr for Resume {
    type Err = core::convert::Infallible;

    fn from_str(said: &str) -> Result<Self, Self::Err> {
        Ok(match said {
            "last" => Self::Last,
            file => Self::File(PathBuf::from(file)),
        })
    }
}

/// How long a process waits for a journal that another process has
/// open, when it cannot do without it. A process that was killed has let
/// go of the file by the time it is gone on most systems, and a moment
/// later on some.
const PATIENCE: Duration = Duration::from_secs(3);

/// A journal in a file, and the file next to it.
struct InFile {
    journal: Arc<LocalJournal>,
    /// Where the journal is.
    path: PathBuf,
    /// The file that holds the token of the call that was last begun.
    last: PathBuf,
}

/// What is held, where sessions are kept in memory, of a session that
/// somebody is to come back to: the hash of its token, and its label.
#[derive(Default)]
struct Held {
    token: Option<[u8; 32]>,
    label: Option<String>,
}

/// Where sessions are kept.
pub struct Kept {
    journal: Arc<dyn Journal>,
    file: Option<InFile>,
    /// What a journal in a file would hold beside the records, where
    /// there is none.
    held: Mutex<BTreeMap<SessionId, Held>>,
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
    /// Opens what `keeps` says sessions are kept in. Returns it, and
    /// what whoever runs the program should know about it. `needed` is
    /// whether the program cannot do without the file: it is to go on
    /// with a session that is in it. `said` is what the settings are
    /// called, for a failure to say what to change.
    ///
    /// # Errors
    ///
    /// The file cannot be opened. A file that cannot be read is left as
    /// it is, and the failure says which file it was.
    pub fn open(
        keeps: &Keeps,
        said: &Named,
        halt: &Halt,
        needed: bool,
    ) -> Result<(Self, Vec<String>), Failure> {
        Self::open_within(keeps, said, halt, needed, PATIENCE)
    }

    /// As [`open`](Self::open), waiting no longer than `patience` for a
    /// file that another process has open.
    fn open_within(
        keeps: &Keeps,
        said: &Named,
        halt: &Halt,
        needed: bool,
        patience: Duration,
    ) -> Result<(Self, Vec<String>), Failure> {
        let (named, path) = match keeps {
            Keeps::Memory => return Ok((Self::in_memory(), Vec::new())),
            Keeps::File(path) => (true, path.clone()),
            Keeps::Usual => (false, PathBuf::from(JOURNAL)),
        };
        if !named && let Some(folder) = path.parent() {
            make_folder(folder).with(|| format!("{} cannot be made", folder.display()))?;
        }
        // A file that the command was told to use, or needs, is waited
        // for a little: the process that had it may only just have
        // died. The usual file of a command that can do without is not.
        let until = Instant::now() + patience;
        let opened = loop {
            let config = Config {
                probe: halt.probe(),
                ..Config::default()
            };
            match LocalJournal::open(&path, config) {
                Err(OpenError::InUse) if (named || needed) && Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                opened => break opened,
            }
        };
        match opened {
            Ok(journal) => {
                let journal = Arc::new(journal);
                let kept = Self {
                    journal: Arc::clone(&journal) as Arc<dyn Journal>,
                    file: Some(InFile {
                        journal,
                        last: last_of(&path),
                        path,
                    }),
                    held: Mutex::default(),
                };
                Ok((kept, Vec::new()))
            }
            // Two calls at once from one folder are both taken. The
            // second is told what it does without.
            Err(OpenError::InUse) if !named && !needed => {
                let note = format!(
                    "another waveport has {} open: this one keeps its calls in memory, and one \
                     that breaks off is gone ({} names another file)",
                    path.display(),
                    said.journal
                );
                Ok((Self::in_memory(), vec![note]))
            }
            Err(error) => Err(Failure(format!(
                "the calls kept in {} cannot be opened: {error}. The file was left as it is: \
                 {} names another, and {} keeps calls in memory",
                path.display(),
                said.journal,
                said.memory
            ))),
        }
    }

    fn in_memory() -> Self {
        Self {
            journal: Arc::new(MemoryJournal::new()),
            file: None,
            held: Mutex::default(),
        }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, Held>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A token for whoever is to come back to the session `id`, in
    /// place of the one before: the far end of a session, whose line
    /// may drop. Where sessions are kept in a file it also lets the
    /// session be gone on with by the process after this one. Where
    /// they are kept in memory it is good for as long as this process.
    ///
    /// # Errors
    ///
    /// The file cannot be written to, or the operating system gave no
    /// entropy.
    pub async fn credential(&self, id: SessionId) -> Result<ResumeToken, Failure> {
        if let Some(token) = self.issue(id).await? {
            return Ok(token);
        }
        let token = ResumeToken::new(id).with(|| "no token could be made".to_owned())?;
        self.held().entry(id).or_default().token = Some(token.hash());
        Ok(token)
    }

    /// Whether `token` is the one that was last made for its session,
    /// here or by a process before this one.
    ///
    /// # Errors
    ///
    /// The file cannot be read.
    pub async fn admits(&self, token: &ResumeToken) -> Result<bool, Failure> {
        if self.file.is_some() {
            return Ok(self.admit(token).await?.is_some());
        }
        let held = self.held();
        let hash = held.get(&token.session()).and_then(|held| held.token);
        Ok(hash.is_some_and(|hash| token.is(&hash)))
    }

    /// Keeps `label` with the session `id`: what whoever goes on with
    /// the session has to know of it, such as which agent it is a
    /// session of.
    ///
    /// # Errors
    ///
    /// The file cannot be written to, or the label is longer than a
    /// label may be.
    pub async fn label(&self, id: SessionId, label: &str) -> Result<(), Failure> {
        let Some(file) = &self.file else {
            self.held().entry(id).or_default().label = Some(label.to_owned());
            return Ok(());
        };
        file.journal
            .label(id, label.as_bytes())
            .await
            .with(|| format!("{} cannot be written to", file.path.display()))
    }

    /// The sessions that are kept, each with its label if it has one:
    /// the ones that run, and the ones that broke off and can be gone on
    /// with.
    ///
    /// # Errors
    ///
    /// The file cannot be read.
    pub async fn labelled(&self) -> Result<Vec<(SessionId, Option<String>)>, Failure> {
        let Some(file) = &self.file else {
            let held = self.held();
            return Ok(held
                .iter()
                .map(|(id, held)| (*id, held.label.clone()))
                .collect());
        };
        let kept = file
            .journal
            .labelled()
            .await
            .with(|| format!("{} cannot be read", file.path.display()))?;
        Ok(kept
            .into_iter()
            .map(|(id, label)| (id, label.and_then(|label| String::from_utf8(label).ok())))
            .collect())
    }

    /// The journal, for a session to write to.
    #[must_use]
    pub fn journal(&self) -> Arc<dyn Journal> {
        Arc::clone(&self.journal)
    }

    /// The token that lets the session `id` be gone on with, if
    /// sessions are kept where that can be done.
    ///
    /// # Errors
    ///
    /// The file cannot be written to.
    pub async fn issue(&self, id: SessionId) -> Result<Option<ResumeToken>, Failure> {
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

    /// Makes `token` the one that [`Resume::Last`] finds.
    ///
    /// # Errors
    ///
    /// The file next to the journal cannot be written.
    pub fn remember(&self, token: &ResumeToken) -> Result<(), Failure> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        write_private(&file.last, &format!("{token}\n"))
            .with(|| format!("{} cannot be written", file.last.display()))
    }

    /// The token that `from` names.
    ///
    /// # Errors
    ///
    /// Sessions are kept in memory, there is no session to go on with,
    /// or the file holds something that is not a token.
    pub fn token(&self, from: &Resume) -> Result<ResumeToken, Failure> {
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

    /// The session that `token` lets its holder go on with, if the
    /// journal has such a session.
    ///
    /// # Errors
    ///
    /// The file cannot be read.
    pub async fn admit(&self, token: &ResumeToken) -> Result<Option<SessionId>, Failure> {
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

    /// The session to go on with, as `from` names it.
    ///
    /// # Errors
    ///
    /// There is no token as `from` says, or the session it names is not
    /// in the journal.
    pub async fn resume(&self, from: &Resume) -> Result<SessionId, Failure> {
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

    /// What whoever runs the program is told of the sessions that broke
    /// off earlier and are still kept, if there are any: nothing takes
    /// them out of the file but going on with them to their end.
    pub async fn earlier(&self) -> Option<String> {
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

    /// Whether a session that broke off here can be gone on with.
    #[must_use]
    pub const fn outlasts_the_program(&self) -> bool {
        self.file.is_some()
    }

    /// Lets go of the session `id`, which is over: nobody goes on with
    /// it.
    ///
    /// # Errors
    ///
    /// The session could not be taken out of the file. It is no longer
    /// the one that [`Resume::Last`] finds either way.
    pub async fn forget(&self, id: SessionId) -> Result<(), Failure> {
        let Some(file) = &self.file else {
            self.held().remove(&id);
            return Ok(());
        };
        let forgotten = file.journal.forget(id).await.map_err(|error| {
            Failure(format!(
                "the call could not be taken out of {}: {error}",
                file.path.display()
            ))
        });
        // The session that was last begun is no longer one to go on
        // with.
        let last = std::fs::read_to_string(&file.last)
            .ok()
            .and_then(|text| text.parse::<ResumeToken>().ok());
        if last.is_some_and(|token| token.session() == id) {
            let _ = std::fs::remove_file(&file.last);
        }
        forgotten
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use waveport_ports::journal::SessionId;

    use super::{Kept, Resume, last_of, write_private};
    use crate::halt::Halt;
    use crate::spec::{Keeps, Named};

    /// What a command line calls the settings.
    const SAID: Named = Named {
        journal: "--journal",
        memory: "--no-journal",
        assets: "--assets",
        otlp: "--otlp",
        by: "the command line",
    };

    /// A folder for what a test writes, empty.
    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir()
            .join(format!("waveport-assembly-{}", std::process::id()))
            .join(name);
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn kept_in(journal: &Path) -> Kept {
        let file = Keeps::File(journal.to_owned());
        let (kept, notes) = Kept::open(&file, &SAID, &Halt::Nowhere, false).unwrap();
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
    async fn whoever_is_to_come_back_has_a_token_wherever_sessions_are_kept() {
        let folder = folder("credentials");
        let journal = folder.join("calls.redb");
        let memory = Kept::open(&Keeps::Memory, &SAID, &Halt::Nowhere, false)
            .unwrap()
            .0;
        for kept in [kept_in(&journal), memory] {
            let (a, b) = (SessionId(0xA), SessionId(0xB));
            let first = kept.credential(a).await.unwrap();
            assert_eq!(first.session(), a);
            assert!(kept.admits(&first).await.unwrap());
            // A new one takes the place of the one before, and a token
            // is good for its own session alone.
            let second = kept.credential(a).await.unwrap();
            assert!(!kept.admits(&first).await.unwrap());
            assert!(kept.admits(&second).await.unwrap());
            let other = kept.credential(b).await.unwrap();
            assert!(kept.admits(&other).await.unwrap());
            let forged = format!("wpr1_{:032x}{}", a.0, "0".repeat(32));
            assert!(!kept.admits(&forged.parse().unwrap()).await.unwrap());

            // What is kept with a session says what it is a session of.
            kept.label(a, "front-desk").await.unwrap();
            assert_eq!(
                kept.labelled().await.unwrap(),
                [(a, Some("front-desk".to_owned())), (b, None)]
            );
            // A session that is over is let go of, with all of it.
            kept.forget(a).await.unwrap();
            assert!(!kept.admits(&second).await.unwrap());
            assert_eq!(kept.labelled().await.unwrap(), [(b, None)]);
        }

        // In a file, both are there for the process after this one.
        let kept = kept_in(&journal);
        let token = kept.credential(SessionId(0xC)).await.unwrap();
        kept.label(SessionId(0xC), "night-desk").await.unwrap();
        drop(kept);
        let kept = kept_in(&journal);
        assert!(kept.admits(&token).await.unwrap());
        assert_eq!(
            kept.labelled().await.unwrap(),
            [
                (SessionId(0xB), None),
                (SessionId(0xC), Some("night-desk".to_owned()))
            ]
        );
    }

    #[tokio::test]
    async fn a_call_that_was_begun_is_one_to_go_on_with_until_it_is_over() {
        let folder = folder("begun");
        let journal = folder.join("calls.redb");
        let kept = kept_in(&journal);
        assert!(kept.outlasts_the_program());
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
        kept.forget(id).await.unwrap();
        let Err(over) = kept.resume(&Resume::File(elsewhere)).await else {
            panic!("the call is over");
        };
        assert!(over.0.contains("it ended"), "{}", over.0);
        assert!(last.exists());
        kept.forget(newer).await.unwrap();
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
        let (kept, notes) = Kept::open(&Keeps::Memory, &SAID, &Halt::Nowhere, false).unwrap();
        assert!(notes.is_empty() && !kept.outlasts_the_program());
        assert_eq!(kept.earlier().await, None);
        assert_eq!(kept.issue(SessionId(1)).await.unwrap(), None);
        let Err(failure) = kept.resume(&Resume::Last).await else {
            panic!("nothing is kept");
        };
        assert!(failure.0.contains("kept in memory"), "{}", failure.0);
        kept.forget(SessionId(1)).await.unwrap();
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_read_is_left_as_it_is() {
        let folder = folder("unreadable");
        let journal = folder.join("calls.redb");
        let text = "Not a journal: a letter that was saved under the wrong name.\n".repeat(200);
        std::fs::write(&journal, &text).unwrap();
        let letter = Keeps::File(journal.clone());
        let Err(failure) = Kept::open(&letter, &SAID, &Halt::Nowhere, false).map(|_| ()) else {
            panic!("it is not a journal");
        };
        assert!(failure.0.contains("was left as it is"), "{}", failure.0);
        // And it says what to change, as whoever gave the settings
        // calls it.
        assert!(
            failure
                .0
                .ends_with("--journal names another, and --no-journal keeps calls in memory"),
            "{}",
            failure.0
        );
        assert!(failure.0.contains("calls.redb"), "{}", failure.0);
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), text);

        // A file that another command has open, when it was named: it
        // is waited for, and not for long.
        let journal = folder.join("busy.redb");
        let first = kept_in(&journal);
        let named = Keeps::File(journal.clone());
        let began = Instant::now();
        let patience = Duration::from_millis(300);
        let opened = Kept::open_within(&named, &SAID, &Halt::Nowhere, false, patience);
        let Err(failure) = opened.map(|_| ()) else {
            panic!("the file is in use");
        };
        assert!(failure.0.contains("cannot be opened"), "{}", failure.0);
        assert!(began.elapsed() >= patience, "{:?}", began.elapsed());

        // It is there once the other command has let go of it, which a
        // command that was killed does a moment after it is gone.
        let letting_go = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(first);
        });
        let patience = Duration::from_secs(20);
        let (kept, notes) =
            Kept::open_within(&named, &SAID, &Halt::Nowhere, false, patience).unwrap();
        assert!(notes.is_empty() && kept.outlasts_the_program());
        letting_go.join().unwrap();
    }

    #[test]
    fn the_usual_file_is_done_without_unless_it_is_needed() {
        // A command that is told nothing keeps its calls where it is
        // run, which a test cannot choose. So the usual file is stood in
        // for by one that is named, and the rule is asked directly: a
        // file in use is an error when it is named or needed.
        let folder = folder("usual");
        let journal = folder.join("calls.redb");
        let first = kept_in(&journal);
        let named = Keeps::File(journal.clone());
        let none = Duration::ZERO;
        for needed in [false, true] {
            let opened = Kept::open_within(&named, &SAID, &Halt::Nowhere, needed, none);
            let Err(failure) = opened.map(|_| ()) else {
                panic!("the file is in use");
            };
            assert!(failure.0.contains("in use"), "{}", failure.0);
        }
        drop(first);
    }
}
