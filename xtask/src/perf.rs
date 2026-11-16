//! `cargo xtask perf`: the numbers of one machine, recorded and checked.
//!
//! How much of a thread a session takes, and how long a session takes to
//! start, are numbers about the machine they are measured on. A shared
//! runner cannot hold them, so they are not a gate of every pull request.
//! They are recorded on a machine that is left alone, in
//! `perf/baselines/<machine>.toml`, and checked on the same machine
//! against that record and against the targets in `perf/targets.toml`.
//!
//! The measuring is done by a test of the benchmarks package, which
//! prints each number on a line that begins with `PERF`.
//!
//! A thread that the system moves from one core to another measures the
//! cores as much as the code, most of all where the cores are not alike.
//! So a record can be made with the measurement kept on one processor,
//! `--cpu <n>`, and a check is then made on the same one. That needs
//! `taskset`, which Linux has.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::args::Args;
use crate::error::{Context, Error, Result};
use crate::meta::{self, LoadOptions};

/// The test that measures.
const MEASURE: &[&str] = &[
    "test",
    "--release",
    "--locked",
    "-p",
    "waveport-benches",
    "--test",
    "load",
    "--",
    "--ignored",
    "--nocapture",
    // One measurement at a time: two would measure each other.
    "--test-threads",
    "1",
];

/// How many times a check measures before it says that a machine does
/// worse than its record.
const TRIES: u32 = 3;

/// The number that says which of two measurements is the better one:
/// the share of a thread that a session takes.
const SHARE: &str = "density.share_percent";

/// Where the gates' numbers are, from the root of the repository.
const TARGETS: &str = "perf/targets.toml";

/// Runs `record` or `check`.
pub(crate) fn run(args: &[String]) -> Result<bool> {
    let mut args = Args::new(args);
    let named = args.value("--host")?;
    let cpu = match args.value("--cpu")? {
        Some(cpu) => Some(cpu.parse::<u32>().ok().ok_or_else(|| {
            Error::new(format!(
                "`--cpu` takes the number of a processor, not `{cpu}`"
            ))
        })?),
        None => None,
    };
    let rest = args.finish()?;
    let workspace = meta::load(&LoadOptions::default())?;
    let machine = match named {
        Some(name) => slug(&name),
        None => this_machine()?,
    };
    let file = workspace
        .root
        .join("perf/baselines")
        .join(format!("{machine}.toml"));
    match rest.as_slice() {
        [what] if what == "record" => {
            let measured = measure(&workspace.root, cpu)?;
            steady(&measured, &rules(&workspace.root)?)?;
            if let Some(folder) = file.parent() {
                std::fs::create_dir_all(folder)
                    .context(format!("could not make {}", folder.display()))?;
            }
            std::fs::write(&file, written(&machine, cpu, &measured))
                .context(format!("could not write {}", file.display()))?;
            println!(
                "perf: recorded {} numbers in {}",
                measured.len(),
                file.display()
            );
            Ok(true)
        }
        [what] if what == "check" => {
            let (pinned, recorded) = recorded(&file)?;
            if cpu.is_some() && cpu != pinned {
                // A check on another processor than the record's, or on
                // one when the record was made on any, compares two
                // different things.
                return Err(Error::new(format!(
                    "a check is made the way its record was, and {} says: {}",
                    file.display(),
                    pinned.map_or_else(
                        || "on no processor in particular".to_owned(),
                        |cpu| format!("on processor {cpu}")
                    )
                )));
            }
            let rules = rules(&workspace.root)?;
            let mut measured = measure(&workspace.root, pinned)?;
            steady(&measured, &rules)?;
            let mut tries = 1;
            loop {
                let (table, held) = compare(&recorded, &measured, &rules);
                if held || tries == TRIES {
                    print!("{table}");
                    if !held {
                        eprintln!(
                            "error[PERF001]: in {TRIES} measurements this machine did worse than \
                             its record in {} allows",
                            file.display()
                        );
                    }
                    return Ok(held);
                }
                // Whatever else the machine does can only make a
                // measurement worse, so one that fails is made again and
                // the better of the two is kept. A change that costs
                // something costs it every time.
                println!("perf: worse than the record allows, so once more");
                let again = measure(&workspace.root, pinned)?;
                steady(&again, &rules)?;
                measured = better(measured, again);
                tries += 1;
            }
        }
        _ => Err(Error::new(
            "usage: cargo xtask perf <record|check> [--host <name>] [--cpu <n>]",
        )),
    }
}

/// What a check holds the numbers to.
#[derive(Debug, PartialEq)]
struct Rules {
    /// How much worse than its record a number may be, as a share of it.
    tolerance: f64,
    /// The numbers that are held to their record. Less is better for
    /// every one of them.
    held: Vec<String>,
    /// The numbers that are also held to a target, whatever the record.
    targets: BTreeMap<String, f64>,
}

/// Reads the rules from `perf/targets.toml`.
fn rules(root: &Path) -> Result<Rules> {
    let file = root.join(TARGETS);
    let text =
        std::fs::read_to_string(&file).context(format!("could not read {}", file.display()))?;
    rules_from(&text).ok_or_else(|| {
        Error::new(format!(
            "{} has no [local] with `tolerance`, `held` and [local.targets]",
            file.display()
        ))
    })
}

fn rules_from(text: &str) -> Option<Rules> {
    let table: toml::Table = toml::from_str(text).ok()?;
    let local = table.get("local")?;
    let held = local.get("held")?.as_array()?;
    let targets = local.get("targets")?.as_table()?;
    Some(Rules {
        tolerance: local.get("tolerance")?.as_float()?,
        held: held
            .iter()
            .filter_map(|name| name.as_str().map(str::to_owned))
            .collect(),
        targets: targets
            .iter()
            .filter_map(|(name, target)| Some((name.clone(), number(target)?)))
            .collect(),
    })
}

/// A number of a TOML file, written whole or not.
fn number(value: &toml::Value) -> Option<f64> {
    #[expect(
        clippy::cast_precision_loss,
        reason = "the numbers of a record are small"
    )]
    let whole = value.as_integer().map(|whole| whole as f64);
    value.as_float().or(whole)
}

/// The command that measures: `cargo`, kept on the processor `cpu` by
/// `taskset` where one is named.
fn command(cpu: Option<u32>) -> Command {
    let cargo = meta::cargo();
    let Some(cpu) = cpu else {
        return cargo;
    };
    let mut pinned = Command::new("taskset");
    pinned
        .arg("-c")
        .arg(cpu.to_string())
        .arg(cargo.get_program());
    pinned
}

/// Runs the test that measures, on the processor `cpu` where one is
/// named, and returns the numbers it printed.
fn measure(root: &Path, cpu: Option<u32>) -> Result<BTreeMap<String, f64>> {
    match cpu {
        Some(cpu) => println!("perf: measuring on processor {cpu}, which takes about a minute"),
        None => println!("perf: measuring, which takes about a minute once it is built"),
    }
    let output = command(cpu)
        .current_dir(root)
        // Not the gate of the nightly run: the numbers are judged here.
        .env_remove("WAVEPORT_PERF_GATE")
        .args(MEASURE)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context(match cpu {
            Some(_) => "could not run `taskset`, which `--cpu` needs",
            None => "could not run `cargo test`",
        })?;
    let printed = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(Error::new(format!("the measurement failed:\n{printed}")));
    }
    let measured = parse(&printed);
    if measured.is_empty() {
        return Err(Error::new("the measurement printed no numbers"));
    }
    Ok(measured)
}

/// The numbers among `printed`: what follows `PERF ` on a line, as
/// `<name>=<number>`. The test harness may have written on the same
/// line before it.
fn parse(printed: &str) -> BTreeMap<String, f64> {
    printed
        .lines()
        .filter_map(|line| Some(line.split_once("PERF ")?.1))
        .filter_map(|line| {
            let (name, value) = line.split_once('=')?;
            Some((name.trim().to_owned(), value.trim().parse().ok()?))
        })
        .collect()
}

/// Refuses numbers that were measured while the machine changed its
/// speed: the reference work took longer at one end of the run than at
/// the other by more than the tolerance.
fn steady(measured: &BTreeMap<String, f64>, rules: &Rules) -> Result<()> {
    match measured.get("density.reference_drift") {
        Some(drift) if *drift > 1.0 + rules.tolerance => Err(Error::new(format!(
            "the reference work took {drift:.2} times as long at one end of the run as at the \
             other: the machine was not left alone. Measure again"
        ))),
        _ => Ok(()),
    }
}

/// The better of two measurements, whole: the one in which a session
/// took less of its thread.
fn better(one: BTreeMap<String, f64>, other: BTreeMap<String, f64>) -> BTreeMap<String, f64> {
    match (one.get(SHARE), other.get(SHARE)) {
        (Some(first), Some(second)) if second < first => other,
        _ => one,
    }
}

/// The record of a machine, as it is written.
fn written(machine: &str, cpu: Option<u32>, measured: &BTreeMap<String, f64>) -> String {
    let mut text = format!(
        "# The numbers of one machine, recorded by `cargo xtask perf record`.\n\
         # `cargo xtask perf check` holds the same machine to them.\n\
         machine = \"{machine}\"\n"
    );
    if let Some(cpu) = cpu {
        let _ = writeln!(
            text,
            "# The processor the measurement was kept on, and a check is kept on.\ncpu = {cpu}"
        );
    }
    text.push_str("\n[measured]\n");
    for (name, value) in measured {
        let _ = writeln!(text, "\"{name}\" = {value}");
    }
    text
}

/// Reads the record of a machine: the processor its measurement was
/// kept on, if it was kept on one, and the numbers.
fn recorded(file: &Path) -> Result<(Option<u32>, BTreeMap<String, f64>)> {
    let text = std::fs::read_to_string(file).context(format!(
        "{} is not there (`cargo xtask perf record` makes it)",
        file.display()
    ))?;
    let table: toml::Table =
        toml::from_str(&text).context(format!("{} is not TOML", file.display()))?;
    let measured = table.get("measured").and_then(toml::Value::as_table);
    let measured =
        measured.ok_or_else(|| Error::new(format!("{} has no [measured]", file.display())))?;
    let cpu = table
        .get("cpu")
        .and_then(toml::Value::as_integer)
        .and_then(|cpu| u32::try_from(cpu).ok());
    let measured = measured
        .iter()
        .filter_map(|(name, value)| Some((name.clone(), number(value)?)))
        .collect();
    Ok((cpu, measured))
}

/// Holds what was measured against the record and the targets. Returns
/// what to print, and whether everything held.
fn compare(
    recorded: &BTreeMap<String, f64>,
    measured: &BTreeMap<String, f64>,
    rules: &Rules,
) -> (String, bool) {
    let mut table = format!(
        "{:<28} {:>10} {:>10} {:>8}  {}\n",
        "number", "recorded", "measured", "change", ""
    );
    let mut held = true;
    for (name, now) in measured {
        let before = recorded.get(name).copied();
        let change = before
            .filter(|before| *before > 0.0)
            .map(|before| (now - before) / before);
        let mut verdict = String::new();
        if rules.held.contains(name) {
            match (before, change) {
                (None, _) => {
                    held = false;
                    verdict.push_str("not in the record");
                }
                (_, Some(change)) if change > rules.tolerance => {
                    held = false;
                    let _ = write!(
                        verdict,
                        "worse by more than {:.0}%",
                        rules.tolerance * 100.0
                    );
                }
                _ => verdict.push_str("held"),
            }
        }
        if let Some(target) = rules.targets.get(name) {
            if *now > *target {
                held = false;
                let _ = write!(verdict, " over its target of {target}");
            } else {
                let _ = write!(verdict, " within its target of {target}");
            }
        }
        let before = before.map_or_else(|| "-".to_owned(), |before| format!("{before:.4}"));
        let change = change.map_or_else(
            || "-".to_owned(),
            |change| format!("{:+.1}%", change * 100.0),
        );
        let _ = writeln!(
            table,
            "{name:<28} {before:>10} {now:>10.4} {change:>8}  {}",
            verdict.trim()
        );
    }
    (table, held)
}

/// A name for this machine: what its processor is called. A name that
/// says nothing of who owns the machine, since records are committed.
fn this_machine() -> Result<String> {
    let found = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            let line = text.lines().find(|line| line.starts_with("model name"))?;
            Some(slug(line.split_once(':')?.1))
        })
        .filter(|name| !name.is_empty());
    found.ok_or_else(|| {
        Error::new(
            "this system does not say what its processor is called: name the machine with \
             `--host <name>`",
        )
    })
}

/// A name as a file can have it: lower case, with a dash for whatever is
/// neither a letter nor a digit, and without the words every processor's
/// name has.
fn slug(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let cleaned = ["(r)", "(tm)", "intel", "amd", "cpu", "processor"]
        .iter()
        .fold(lower, |name, word| name.replace(word, " "));
    // Nothing of a clock speed, which a name sometimes ends with.
    let cleaned = cleaned.split('@').next().unwrap_or_default();
    let words = cleaned
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty());
    words.collect::<Vec<_>>().join("-")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::{
        Rules, better, command, compare, parse, recorded, rules_from, slug, steady, written,
    };

    fn numbers(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), *value))
            .collect()
    }

    fn rules() -> Rules {
        Rules {
            tolerance: 0.15,
            held: vec![
                "density.share_percent".to_owned(),
                "start.p50_ms".to_owned(),
            ],
            targets: numbers(&[("start.p50_ms", 2.0)]),
        }
    }

    #[test]
    fn the_numbers_a_measurement_prints_are_read() {
        let printed = "running 1 test\ntest density ... PERF density.sessions=50\n\
                       PERF density.share_percent=0.3125\nnoise\n  \
                       PERF start.p50_ms = 0.42\nPERF broken\nPERF nothing=\ntest density ... ok\n";
        assert_eq!(
            parse(printed),
            numbers(&[
                ("density.sessions", 50.0),
                ("density.share_percent", 0.3125),
                ("start.p50_ms", 0.42)
            ])
        );
    }

    #[test]
    fn a_machine_is_named_after_its_processor() {
        assert_eq!(slug(" Intel(R) Core(TM) Ultra 7 155H"), "core-ultra-7-155h");
        assert_eq!(
            slug("Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz"),
            "xeon-platinum-8370c"
        );
        assert_eq!(slug("AMD EPYC 7763 64-Core Processor"), "epyc-7763-64-core");
        assert_eq!(slug("My Desk"), "my-desk");
    }

    #[test]
    fn a_record_is_read_as_it_was_written() {
        let measured = numbers(&[("density.share_percent", 0.3125), ("start.p50_ms", 0.42)]);
        let folder = std::env::temp_dir().join(format!("waveport-perf-{}", std::process::id()));
        let file = folder.join("core-ultra-7-155h.toml");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(&file, written("core-ultra-7-155h", None, &measured)).unwrap();
        assert_eq!(recorded(&file).unwrap(), (None, measured.clone()));
        // And with the processor it was measured on.
        std::fs::write(&file, written("core-ultra-7-155h", Some(6), &measured)).unwrap();
        assert_eq!(recorded(&file).unwrap(), (Some(6), measured));
        let _ = std::fs::remove_dir_all(&folder);

        // A machine that was never recorded is told how to be.
        let error = recorded(Path::new("/nowhere/at-all.toml")).unwrap_err();
        assert!(error.to_string().contains("perf record"), "{error}");
    }

    #[test]
    fn a_number_that_is_worse_than_its_record_allows_fails() {
        let recorded = numbers(&[("density.share_percent", 0.30), ("start.p50_ms", 0.40)]);
        // A little worse, and a little better: held.
        let measured = numbers(&[("density.share_percent", 0.34), ("start.p50_ms", 0.38)]);
        let (table, held) = compare(&recorded, &measured, &rules());
        assert!(held, "{table}");
        assert!(
            table.contains("+13.3%") && table.contains("-5.0%"),
            "{table}"
        );

        // Worse by more than the tolerance.
        let measured = numbers(&[("density.share_percent", 0.36), ("start.p50_ms", 0.38)]);
        let (table, held) = compare(&recorded, &measured, &rules());
        assert!(!held && table.contains("worse by more than 15%"), "{table}");

        // Within the record's reach and over the target all the same.
        let recorded = numbers(&[("density.share_percent", 0.30), ("start.p50_ms", 2.4)]);
        let measured = numbers(&[("density.share_percent", 0.30), ("start.p50_ms", 2.5)]);
        let (table, held) = compare(&recorded, &measured, &rules());
        assert!(!held && table.contains("over its target of 2"), "{table}");

        // A number the record does not have cannot be held to it.
        let recorded = numbers(&[("start.p50_ms", 0.40)]);
        let measured = numbers(&[("density.share_percent", 0.30), ("start.p50_ms", 0.40)]);
        let (table, held) = compare(&recorded, &measured, &rules());
        assert!(!held && table.contains("not in the record"), "{table}");

        // A number that is only printed is held to nothing.
        let measured = numbers(&[("poll.longest_ms", 99.0)]);
        assert!(compare(&BTreeMap::new(), &measured, &rules()).1);
    }

    #[test]
    fn a_measurement_is_kept_on_a_processor_by_taskset() {
        let free = command(None);
        assert_ne!(free.get_program(), "taskset");
        let pinned = command(Some(6));
        assert_eq!(pinned.get_program(), "taskset");
        let args: Vec<_> = pinned.get_args().collect();
        assert_eq!(args, ["-c".as_ref(), "6".as_ref(), free.get_program()]);
    }

    #[test]
    fn of_two_measurements_the_one_that_took_less_is_kept_whole() {
        let slow = numbers(&[("density.share_percent", 0.36), ("start.p50_ms", 0.1)]);
        let fast = numbers(&[("density.share_percent", 0.31), ("start.p50_ms", 0.2)]);
        assert_eq!(better(slow.clone(), fast.clone()), fast);
        assert_eq!(better(fast.clone(), slow.clone()), fast);
        // One that does not say what a session took is no better.
        let silent = numbers(&[("start.p50_ms", 0.01)]);
        assert_eq!(better(slow.clone(), silent), slow);
    }

    #[test]
    fn a_machine_that_changed_its_speed_has_measured_nothing() {
        let calm = numbers(&[("density.reference_drift", 1.04)]);
        assert!(steady(&calm, &rules()).is_ok());
        let busy = numbers(&[("density.reference_drift", 1.4)]);
        let error = steady(&busy, &rules()).unwrap_err();
        assert!(error.to_string().contains("Measure again"), "{error}");
    }

    /// The nightly run and this command measure with the same test.
    #[test]
    fn the_nightly_run_measures_the_same_way() {
        let workflow = include_str!("../../.github/workflows/scheduled.yml");
        let command = format!("cargo {}", super::MEASURE.join(" "));
        assert!(
            workflow.contains(&command),
            "scheduled.yml does not run `{command}`"
        );
    }

    #[test]
    fn the_rules_are_the_ones_the_repository_has() {
        let text = include_str!("../../perf/targets.toml");
        let rules = rules_from(text).unwrap();
        assert!(rules.tolerance > 0.0 && rules.tolerance < 1.0);
        assert!(rules.held.contains(&"density.ratio".to_owned()));
        assert!(rules.targets.contains_key("start.p99_ms"));
    }
}
