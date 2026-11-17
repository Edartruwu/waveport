//! Where the threads of a daemon run.
//!
//! A daemon whose file names the processors of its shards is looked at
//! from outside, as whoever runs it would look: the system says for each
//! thread of a process which processors it may run on.

use crate::refused_to_start;

/// A configuration that asks no language model anything.
fn asks_nobody(more: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\n{more}\n[agents.desk]\n[agents.desk.llm]\n\
         url = \"http://127.0.0.1:9/v1\"\n"
    )
}

#[test]
fn a_daemon_is_not_started_with_shards_that_have_no_processor_each() {
    for (more, why) in [
        (
            "shards = 2\nprocessors = [0]\n",
            "`processors` names 1 for 2 `shards`: each shard has one",
        ),
        (
            "shards = 2\nprocessors = [1, 1]\n",
            "`processors` names processor 1 for two shards",
        ),
    ] {
        let (status, said) = refused_to_start(&asks_nobody(more), &[]);
        assert_eq!(status.code(), Some(1), "{said}");
        assert!(said.contains(why), "{said}");
    }
}

/// Anywhere but on Linux there is no call that keeps a thread on a set
/// of processors, and a daemon that is told to keep its shards somewhere
/// says so. It does not run as if it had.
#[cfg(not(target_os = "linux"))]
#[test]
fn a_daemon_that_cannot_place_its_shards_does_not_start() {
    let (status, said) = refused_to_start(&asks_nobody("processors = [0]\n"), &[]);
    assert_eq!(status.code(), Some(1), "{said}");
    let why = "waveportd: the shards cannot have the processors that `processors` names: a \
               thread can be kept on a processor on Linux only";
    assert!(said.contains(why), "{said}");
    // The file itself holds together, wherever it is read.
    let desk = crate::desk();
    let checked = crate::command(&desk, &asks_nobody("processors = [0]\n"), &[])
        .arg("--check")
        .output()
        .unwrap();
    assert!(checked.status.success());
}

#[cfg(target_os = "linux")]
mod on_linux {
    use std::path::Path;

    use waveport_testkit::servers::HttpServer;

    use super::asks_nobody;
    use crate::{ANSWER, Served, answer, config, desk, is_fact, refused_to_start};

    /// Processors, from how the system lists them: `0-3,6`.
    fn processors(listed: &str) -> Vec<usize> {
        let mut all = Vec::new();
        for part in listed.trim().split(',').filter(|part| !part.is_empty()) {
            let (first, last) = part.split_once('-').unwrap_or((part, part));
            let (first, last): (usize, usize) = (first.parse().unwrap(), last.parse().unwrap());
            all.extend(first..=last);
        }
        all
    }

    /// The processors the thread whose folder is `task` may run on, if
    /// the thread is still there.
    fn allowed(task: &Path) -> Option<Vec<usize>> {
        let status = std::fs::read_to_string(task.join("status")).ok()?;
        let listed = status
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))?;
        Some(processors(listed))
    }

    /// Every thread of the process `pid`: its name, and the processors
    /// it may run on.
    fn threads_of(pid: u32) -> Vec<(String, Vec<usize>)> {
        let mut threads = Vec::new();
        for task in std::fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
            let task = task.unwrap().path();
            // A thread may have ended since it was listed.
            let Ok(name) = std::fs::read_to_string(task.join("comm")) else {
                continue;
            };
            if let Some(allowed) = allowed(&task) {
                threads.push((name.trim().to_owned(), allowed));
            }
        }
        threads
    }

    #[tokio::test]
    async fn the_shards_have_processors_of_their_own_and_no_other_thread_is_on_them() {
        // The daemon begins where the thread that starts it is.
        let all = allowed(Path::new("/proc/thread-self")).unwrap();
        let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
        // As many shards as leave a processor to the other threads, and
        // two at most. They are named from the far end, so that they
        // are not the ones a thread is on by itself.
        let shards: Vec<usize> = all.iter().rev().take(2).copied().collect();
        let shards = shards.get(..shards.len().min(all.len() - 1)).unwrap();
        let Some(first) = shards.first() else {
            // There is one processor. It cannot be a shard's, because
            // the other threads have to run somewhere.
            let more = format!("processors = [{}]\n", all[0]);
            let (status, said) = refused_to_start(&config(&llm, &more), &[]);
            assert_eq!(status.code(), Some(1), "{said}");
            assert!(said.contains("its other threads need one too"), "{said}");
            return;
        };
        let rest: Vec<usize> = all
            .iter()
            .copied()
            .filter(|processor| !shards.contains(processor))
            .collect();
        let more = format!("shards = {}\nprocessors = {shards:?}\n", shards.len());
        let daemon = Served::start(&desk(), &config(&llm, &more), &[]).await;

        // A session is had, so that every kind of thread the daemon has
        // was needed once: the ones that serve connections, a shard, and
        // whatever the shard starts.
        let (name, token) = daemon.session().await;
        let mut events = daemon.follow(&name, "", Some(&token), &[]).await.unwrap();
        let welcome = events.next().await.unwrap().json();
        let resume = welcome["resume_token"].as_str().unwrap().to_owned();
        daemon.types(&name, &resume, "What time is it?").await;
        let whole = ANSWER.concat();
        events
            .until(|event| is_fact(event, "utterance_committed") && event.json()["text"] == whole)
            .await;

        let threads = threads_of(daemon.child.id());
        // A name of a thread is cut after fifteen letters, and with it
        // the number of the shard.
        let (of_shards, others): (Vec<_>, Vec<_>) = threads
            .iter()
            .partition(|(name, _)| name.starts_with("waveport-shard"));
        let mut theirs: Vec<Vec<usize>> = of_shards
            .iter()
            .map(|(_, allowed)| allowed.clone())
            .collect();
        theirs.sort();
        let mut each: Vec<Vec<usize>> = shards.iter().map(|&processor| vec![processor]).collect();
        each.sort();
        assert_eq!(theirs, each, "{threads:?}");
        // The first thread, the ones that serve connections, the
        // journal's: every one of them is off the shards' processors.
        assert!(others.len() >= 3, "{threads:?}");
        for (name, allowed) in &others {
            assert_eq!(allowed, &rest, "{name}: {threads:?}");
        }
        let said = daemon.noted("waveportd: the shards are kept on").await;
        let processor = if shards.len() == 1 {
            format!("processors {first},")
        } else {
            format!("processors {first}, {},", shards[1])
        };
        assert!(
            said.contains(&format!(
                "waveportd: the shards are kept on {processor} one each"
            )),
            "{said}"
        );
    }

    #[test]
    fn a_daemon_is_not_started_on_a_processor_its_process_may_not_run_on() {
        let all = allowed(Path::new("/proc/thread-self")).unwrap();
        let Some(missing) = (0..1024).rev().find(|processor| !all.contains(processor)) else {
            return;
        };
        let more = format!("processors = [{missing}]\n");
        let (status, said) = refused_to_start(&asks_nobody(&more), &[]);
        assert_eq!(status.code(), Some(1), "{said}");
        let why = format!(
            "waveportd: the shards cannot have the processors that `processors` names: \
             processor {missing} is not one this process may run on, which are "
        );
        assert!(said.contains(&why), "{said}");
    }
}
