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

/// The number that says what state a machine is in: how long the
/// reference work takes it.
const REFERENCE: &str = "density.reference_ms";

/// The number that says which of two measurements is the better one:
/// the share of a thread that a session takes.
const SHARE: &str = "density.share_percent";

/// Where the gates' numbers are, from the root of the repository.
const TARGETS: &str = "perf/targets.toml";

/// Where the system says how the machine is powered.
const POWER: &str = "/sys/class/power_supply";

/// Where the system says how the speed of each processor is governed.
const PROCESSORS: &str = "/sys/devices/system/cpu";

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
            let rules = rules(&workspace.root)?;
            let measured = measure(&workspace.root, cpu)?;
            // Later measurements are held to a record, so it is made on
            // a steadier machine than a check asks for.
            steady(&measured, rules.record_drift)?;
            if let Some(folder) = file.parent() {
                std::fs::create_dir_all(folder)
                    .context(format!("could not make {}", folder.display()))?;
            }
            let state = State {
                power: power(Path::new(POWER)).map(str::to_owned),
                scaling: scaling(Path::new(PROCESSORS), cpu),
            };
            std::fs::write(&file, written(&machine, cpu, &state, &measured))
                .context(format!("could not write {}", file.display()))?;
            println!(
                "perf: recorded {} numbers in {}",
                measured.len(),
                file.display()
            );
            Ok(true)
        }
        [what] if what == "check" => {
            let Record {
                cpu: pinned,
                state: then,
                measured: recorded,
            } = recorded(&file)?;
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
            // Before a minute is spent measuring what cannot be compared.
            let now = scaling(Path::new(PROCESSORS), pinned);
            governed_alike(then.scaling.as_deref(), now.as_deref())?;
            let powers = (then.power.as_deref(), power(Path::new(POWER)));
            let mut measured = measure(&workspace.root, pinned)?;
            steady(&measured, rules.tolerance)?;
            as_recorded(&recorded, &measured, powers, &rules)?;
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
                steady(&again, rules.tolerance)?;
                as_recorded(&recorded, &again, powers, &rules)?;
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
    /// How much longer the reference work may take at one end of the run
    /// than at the other when a record is made, as a share of it.
    record_drift: f64,
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
            "{} has no [local] with `tolerance`, `record_drift`, `held` and [local.targets]",
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
        record_drift: local.get("record_drift")?.as_float()?,
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
/// the other by more than `allowed`, as a share of it. A record is
/// allowed less than a check.
fn steady(measured: &BTreeMap<String, f64>, allowed: f64) -> Result<()> {
    match measured.get("density.reference_drift") {
        Some(drift) if *drift > 1.0 + allowed => Err(Error::new(format!(
            "the reference work took {drift:.3} times as long at one end of the run as at the \
             other, and {:.2} is allowed: the machine was not left alone. Measure again",
            1.0 + allowed
        ))),
        _ => Ok(()),
    }
}

/// How the machine is powered, as the system says under `supplies`:
/// `mains` when something that is not a battery supplies it, `battery`
/// when nothing does and it has one. Nothing where the system does not
/// say, as on a machine without a battery.
fn power(supplies: &Path) -> Option<&'static str> {
    let mut battery = false;
    for supply in std::fs::read_dir(supplies).ok()?.flatten() {
        let said = |what: &str| {
            let text = std::fs::read_to_string(supply.path().join(what)).unwrap_or_default();
            text.trim().to_owned()
        };
        // The battery of a mouse or a keyboard powers nothing here.
        if said("scope") == "Device" {
            continue;
        }
        if said("type") == "Battery" {
            battery = true;
        } else if said("online") == "1" {
            return Some("mains");
        }
    }
    battery.then_some("battery")
}

/// What to say of how a machine was powered for its record and is
/// powered now, where the system said both.
fn powers_said(powers: (Option<&str>, Option<&str>)) -> String {
    let spoken = |power: &str| match power {
        "battery" => "its battery",
        _ => "mains power",
    };
    match powers {
        (Some(then), Some(now)) if then == now => {
            format!(" It is on {} now, as it was then.", spoken(now))
        }
        (Some(then), Some(now)) => format!(
            " Its record was made on {}, and it is on {} now.",
            spoken(then),
            spoken(now)
        ),
        _ => String::new(),
    }
}

/// How the speed of a processor is governed, as the system says under
/// `processors`: its governor, and after a stroke its preference between
/// energy and speed where it has one. It is asked of the processor the
/// measurement is kept on, or of the first. Nothing where the system
/// does not say.
fn scaling(processors: &Path, cpu: Option<u32>) -> Option<String> {
    let folder = processors
        .join(format!("cpu{}", cpu.unwrap_or(0)))
        .join("cpufreq");
    let said = |what: &str| {
        let text = std::fs::read_to_string(folder.join(what)).unwrap_or_default();
        text.trim().to_owned()
    };
    let governor = said("scaling_governor");
    if governor.is_empty() {
        return None;
    }
    let preference = said("energy_performance_preference");
    Some(if preference.is_empty() {
        governor
    } else {
        format!("{governor}/{preference}")
    })
}

/// Refuses to hold a machine to a record that was made with the speed of
/// its processor governed otherwise than it is now.
///
/// How long the reference work takes does not tell the two apart, and
/// what a session takes does. The reference work runs on, and a
/// processor that saves energy is at speed for it soon enough. A session
/// works in bursts of a fraction of a millisecond, and is done before
/// the processor is. The laptop these records began with takes the same
/// time over the reference work either way, and a quarter more of a
/// thread for a session when its power profile is the balanced one.
fn governed_alike(then: Option<&str>, now: Option<&str>) -> Result<()> {
    match (then, now) {
        (Some(then), Some(now)) if then != now => Err(Error::new(format!(
            "the speed of this machine's processor is governed as `{now}` now, and was as \
             `{then}` when its record was made (the governor, and its preference between \
             energy and speed): a session works in bursts, and what it takes of a thread \
             cannot be compared between the two. Set it as it was, which a power profile \
             does where there are such, or make a new record with `cargo xtask perf record`"
        ))),
        _ => Ok(()),
    }
}

/// Refuses to hold a machine to a record that was made in another state
/// of it: the reference work takes it longer or shorter now than then,
/// by more than the tolerance.
///
/// The same machine is not always the same machine. A laptop on its
/// battery, or one that is warm, or one that is doing something else,
/// takes nearly twice as long over the same work, and so over a session.
/// Held to a record made when it was fast, it would fail for no change
/// in the code; held to one made when it was slow, it would pass
/// whatever the change.
///
/// `powers` are how the machine was powered for its record and how it is
/// powered now, which the refusal says where both are known.
fn as_recorded(
    recorded: &BTreeMap<String, f64>,
    measured: &BTreeMap<String, f64>,
    powers: (Option<&str>, Option<&str>),
    rules: &Rules,
) -> Result<()> {
    let (Some(then), Some(now)) = (recorded.get(REFERENCE), measured.get(REFERENCE)) else {
        return Ok(());
    };
    if *then > 0.0 && ((now - then) / then).abs() > rules.tolerance {
        return Err(Error::new(format!(
            "the reference work takes this machine {now} ms now and took it {then} ms when its \
             record was made: it is not in the state it was recorded in (its power, its heat, \
             or what else it is doing).{} Bring it back to that state, or make a new record \
             with `cargo xtask perf record`",
            powers_said(powers)
        )));
    }
    Ok(())
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
fn written(
    machine: &str,
    cpu: Option<u32>,
    state: &State,
    measured: &BTreeMap<String, f64>,
) -> String {
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
    if let Some(power) = &state.power {
        let _ = writeln!(
            text,
            "# How the machine was powered: on `mains`, or on its `battery`.\npower = \"{power}\""
        );
    }
    if let Some(scaling) = &state.scaling {
        let _ = writeln!(
            text,
            "# How the speed of that processor was governed: its governor, and its\n\
             # preference between energy and speed. A check is made with the same.\n\
             scaling = \"{scaling}\""
        );
    }
    text.push_str("\n[measured]\n");
    for (name, value) in measured {
        let _ = writeln!(text, "\"{name}\" = {value}");
    }
    text
}

/// The state a machine was in when its record was made, as far as the
/// system said.
#[derive(Debug, Default, PartialEq)]
struct State {
    /// How it was powered: on `mains` or on its `battery`.
    power: Option<String>,
    /// How the speed of its processor was governed.
    scaling: Option<String>,
}

/// The record of a machine, as it is read.
#[derive(Debug, PartialEq)]
struct Record {
    /// The processor its measurement was kept on, if it was kept on one.
    cpu: Option<u32>,
    state: State,
    measured: BTreeMap<String, f64>,
}

/// Reads the record of a machine.
fn recorded(file: &Path) -> Result<Record> {
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
    let said = |what: &str| {
        table
            .get(what)
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    let state = State {
        power: said("power"),
        scaling: said("scaling"),
    };
    let measured = measured
        .iter()
        .filter_map(|(name, value)| Some((name.clone(), number(value)?)))
        .collect();
    Ok(Record {
        cpu,
        state,
        measured,
    })
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
        Record, Rules, State, as_recorded, better, command, compare, governed_alike, parse, power,
        recorded, rules_from, scaling, slug, steady, written,
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
            record_drift: 0.02,
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
        let silent = State::default();
        std::fs::write(
            &file,
            written("core-ultra-7-155h", None, &silent, &measured),
        )
        .unwrap();
        let plain = Record {
            cpu: None,
            state: State::default(),
            measured: measured.clone(),
        };
        assert_eq!(recorded(&file).unwrap(), plain);
        // And with the processor it was measured on, and the state the
        // machine was in.
        let state = State {
            power: Some("mains".to_owned()),
            scaling: Some("powersave/performance".to_owned()),
        };
        let text = written("core-ultra-7-155h", Some(6), &state, &measured);
        std::fs::write(&file, text).unwrap();
        let whole = Record {
            cpu: Some(6),
            state,
            measured,
        };
        assert_eq!(recorded(&file).unwrap(), whole);
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
        assert!(steady(&calm, rules().tolerance).is_ok());
        let busy = numbers(&[("density.reference_drift", 1.4)]);
        let error = steady(&busy, rules().tolerance).unwrap_err();
        assert!(error.to_string().contains("Measure again"), "{error}");
    }

    #[test]
    fn a_record_is_made_on_a_steadier_machine_than_a_check_asks_for() {
        // What a check lets pass is too much for a record: the record
        // this repository had first was made at 1.076.
        let drifting = numbers(&[("density.reference_drift", 1.076)]);
        assert!(steady(&drifting, rules().tolerance).is_ok());
        let error = steady(&drifting, rules().record_drift).unwrap_err();
        assert!(error.to_string().contains("1.02 is allowed"), "{error}");
        let still = numbers(&[("density.reference_drift", 1.003)]);
        assert!(steady(&still, rules().record_drift).is_ok());
    }

    #[test]
    fn how_a_processor_is_governed_is_read_for_the_one_that_measures() {
        let folder =
            std::env::temp_dir().join(format!("waveport-perf-scaling-{}", std::process::id()));
        let governed = |cpu: u32, said: &[(&str, &str)]| {
            let cpufreq = folder.join(format!("cpu{cpu}")).join("cpufreq");
            std::fs::create_dir_all(&cpufreq).unwrap();
            for (what, text) in said {
                std::fs::write(cpufreq.join(what), format!("{text}\n")).unwrap();
            }
        };
        // A system that says nothing.
        assert_eq!(scaling(&folder, Some(6)), None);
        // A governor alone, and one with a preference.
        governed(0, &[("scaling_governor", "schedutil")]);
        governed(
            6,
            &[
                ("scaling_governor", "powersave"),
                ("energy_performance_preference", "balance_performance"),
            ],
        );
        assert_eq!(scaling(&folder, None).as_deref(), Some("schedutil"));
        assert_eq!(
            scaling(&folder, Some(6)).as_deref(),
            Some("powersave/balance_performance")
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_machine_governed_otherwise_than_for_its_record_is_not_held_to_it() {
        let then = Some("powersave/performance");
        assert!(governed_alike(then, then).is_ok());
        // A record or a system that does not say holds to nothing.
        assert!(governed_alike(None, then).is_ok());
        assert!(governed_alike(then, None).is_ok());
        let error = governed_alike(then, Some("powersave/balance_performance")).unwrap_err();
        let said = error.to_string();
        assert!(
            said.contains("`powersave/balance_performance` now")
                && said.contains("`powersave/performance` when its record was made")
                && said.contains("perf record"),
            "{said}"
        );
    }

    #[test]
    fn how_a_machine_is_powered_is_read_from_what_supplies_it() {
        let folder =
            std::env::temp_dir().join(format!("waveport-perf-power-{}", std::process::id()));
        let supply = |name: &str, said: &[(&str, &str)]| {
            let supply = folder.join(name);
            std::fs::create_dir_all(&supply).unwrap();
            for (what, text) in said {
                std::fs::write(supply.join(what), format!("{text}\n")).unwrap();
            }
        };
        // A system that says nothing.
        assert_eq!(power(&folder), None);
        // The battery of a mouse is not the machine's.
        supply("mouse", &[("type", "Battery"), ("scope", "Device")]);
        assert_eq!(power(&folder), None);
        // A laptop off its charger, and on it.
        supply("BAT0", &[("type", "Battery")]);
        supply("AC", &[("type", "Mains"), ("online", "0")]);
        assert_eq!(power(&folder), Some("battery"));
        supply("AC", &[("type", "Mains"), ("online", "1")]);
        assert_eq!(power(&folder), Some("mains"));
        let _ = std::fs::remove_dir_all(&folder);
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
    fn a_machine_in_another_state_than_its_record_is_not_held_to_it() {
        let recorded = numbers(&[("density.reference_ms", 4.47)]);
        // A little faster or slower: the same machine.
        for now in [4.2, 4.47, 4.8] {
            let measured = numbers(&[("density.reference_ms", now)]);
            let held = as_recorded(&recorded, &measured, (None, None), &rules());
            assert!(held.is_ok(), "{now}");
        }
        // Nearly twice as fast, as when its charger is put in; and slower.
        for now in [2.49, 5.5] {
            let measured = numbers(&[("density.reference_ms", now)]);
            let error = as_recorded(&recorded, &measured, (None, None), &rules()).unwrap_err();
            assert!(error.to_string().contains("perf record"), "{error}");
            assert!(!error.to_string().contains("battery"), "{error}");
        }
        // Where the record and the system say how it is powered, so does
        // the refusal.
        let measured = numbers(&[("density.reference_ms", 2.49)]);
        let powers = (Some("battery"), Some("mains"));
        let error = as_recorded(&recorded, &measured, powers, &rules()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("made on its battery, and it is on mains power now"),
            "{error}"
        );
        let powers = (Some("mains"), Some("mains"));
        let error = as_recorded(&recorded, &measured, powers, &rules()).unwrap_err();
        assert!(
            error.to_string().contains("on mains power now, as it was"),
            "{error}"
        );
        // A record without the number holds to no state.
        assert!(as_recorded(&BTreeMap::new(), &measured, (None, None), &rules()).is_ok());
    }

    #[test]
    fn the_rules_are_the_ones_the_repository_has() {
        let text = include_str!("../../perf/targets.toml");
        let rules = rules_from(text).unwrap();
        assert!(rules.tolerance > 0.0 && rules.tolerance < 1.0);
        assert!(rules.record_drift > 0.0 && rules.record_drift < rules.tolerance);
        assert!(rules.held.contains(&"density.ratio".to_owned()));
        assert!(rules.targets.contains_key("start.p99_ms"));
    }
}
