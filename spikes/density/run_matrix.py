#!/usr/bin/env python3
"""Runs the density spike's measurements and appends one JSON line per run.

    run_matrix.py <bin-dir> <out.jsonl> <part>...

<bin-dir> holds density-mimalloc, density-jemalloc and density-system: the
`density` binary built with each allocator feature. Parts: ramp, knobs,
alloc, soak.

The CPU numbers are for an Intel Core Ultra 7 155H, where logical CPUs 0-11
are six P-cores with two threads each (pairs 0/5, 1/2, 3/4, 6/7, 8/9, 10/11)
and 12-19 are E-cores. Every pinned thread gets a physical core to itself.
Only four cores are loaded at a time: with every P-core busy this laptop
reaches 100 degrees C and throttles to a fraction of its speed, so a layout
with one shard per core measures the cooling, not the code.

A run only counts when the machine was left alone. Before each run the
driver waits for the package to be cool and the machine idle. During the
run it measures how much CPU everything else used. Afterwards it checks the
calibration loop the harness runs on the shard's core before and after. A
run that fails any of the three is repeated; if no attempt is clean, the
least disturbed one is kept and marked.
"""

import json
import os
import resource
import subprocess
import sys
import tempfile
import time

# The calibration loop on a cool, idle P-core of this machine.
CLEAN_CALIBRATION_MS = float(os.environ.get("CLEAN_CALIBRATION_MS", "54.5"))
# Average number of cores other processes may use during a run. An idle
# desktop session on this machine uses about 0.4.
OTHER_LIMIT = float(os.environ.get("OTHER_LIMIT", "1.0"))
QUIET_WAIT = float(os.environ.get("QUIET_WAIT", "150"))
ATTEMPTS = int(os.environ.get("ATTEMPTS", "4"))
MAX_TEMP = 75_000

# One shard on CPU 6, the batcher on CPU 8, end-of-turn threads on CPUs 10
# and 0, the load generator on an E-core.
SINGLE = [
    "--shard-cpus", "6",
    "--batcher-cpus", "8",
    "--turn-cpus", "10,0",
    "--generator-cpus", "12",
]
# Short runs fit the quiet spells of a machine that is also used for other
# work: 40 s at 100 sessions is 200,000 packets.
SHORT = ["--warmup", "10", "--measure", "40"]


def machine_busy():
    """Seconds of CPU time used by every process since boot."""
    with open("/proc/stat", encoding="ascii") as handle:
        fields = handle.readline().split()
    user, nice, system, _idle, _iowait, irq, softirq = (int(f) for f in fields[1:8])
    return (user + nice + system + irq + softirq) / os.sysconf("SC_CLK_TCK")


def package_temp():
    base = "/sys/class/thermal"
    for zone in os.listdir(base):
        try:
            with open(f"{base}/{zone}/type", encoding="ascii") as handle:
                if handle.read().strip() != "x86_pkg_temp":
                    continue
            with open(f"{base}/{zone}/temp", encoding="ascii") as handle:
                return int(handle.read())
        except OSError:
            continue
    return 0


def wait_quiet():
    """Waits for a cool, idle machine. False if it never came."""
    deadline = time.monotonic() + QUIET_WAIT
    cpus = os.cpu_count() or 1
    while time.monotonic() < deadline:
        before = machine_busy()
        time.sleep(3)
        cores = (machine_busy() - before) / 3
        if package_temp() < MAX_TEMP and cores < OTHER_LIMIT and cores < cpus:
            return True
    return False


def children_cpu():
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return usage.ru_utime + usage.ru_stime


def attempt(binary, label, options):
    quiet_start = wait_quiet()
    with tempfile.NamedTemporaryFile(mode="r", suffix=".json") as out:
        busy, own, start = machine_busy(), children_cpu(), time.monotonic()
        subprocess.run([binary, *options, "--label", label, "--out", out.name], check=True)
        elapsed = time.monotonic() - start
        other = (machine_busy() - busy - (children_cpu() - own)) / elapsed
        run = json.loads(out.read())
    slowed = any(max(pair) > CLEAN_CALIBRATION_MS * 1.05 for pair in run["calibration_ms"])
    run["quiet_start"] = quiet_start
    run["other_cores"] = round(max(other, 0.0), 2)
    run["core_slowed"] = slowed
    run["clean"] = quiet_start and not slowed and other <= OTHER_LIMIT
    return run


def disturbance(run):
    """Orders attempts: clean first, then a core at full speed, then quiet."""
    return (not run["clean"], run["core_slowed"], run["other_cores"])


def measure(bin_dir, out_path, allocator, label, options):
    binary = os.path.join(bin_dir, f"density-{allocator}")
    kept = None
    for number in range(1, ATTEMPTS + 1):
        run = attempt(binary, label, options)
        run["attempt"] = number
        print(
            f"  -> attempt {number}: other load {run['other_cores']} cores, "
            f"core slowed: {run['core_slowed']}, clean: {run['clean']}",
            flush=True,
        )
        if kept is None or disturbance(run) < disturbance(kept):
            kept = run
        if run["clean"]:
            break
    with open(out_path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(kept) + "\n")


def main():
    bin_dir, out_path, parts = sys.argv[1], sys.argv[2], sys.argv[3:]
    for part in parts:
        if part == "ramp":
            for sessions in (25, 50, 75, 100, 125, 150, 175, 200):
                options = [*SINGLE, *SHORT, "--sessions", str(sessions)]
                measure(bin_dir, out_path, "mimalloc", "ramp", options)
        elif part == "knobs":
            base = [*SINGLE, *SHORT, "--sessions", "100"]
            for label, extra in (
                ("complexity-5", ["--complexity", "5"]),
                ("greedy", ["--policy", "greedy"]),
                ("energy-gate", ["--energy-gate"]),
                ("complexity-5-energy-gate", ["--complexity", "5", "--energy-gate"]),
            ):
                measure(bin_dir, out_path, "mimalloc", label, base + extra)
        elif part == "alloc":
            for repetition in (1, 2):
                for allocator in ("mimalloc", "jemalloc", "system"):
                    options = [*SINGLE, *SHORT, "--sessions", "100"]
                    measure(bin_dir, out_path, allocator, f"alloc-{repetition}", options)
        elif part == "soak":
            # Allocator behaviour under churn: each session is replaced
            # about every 40 s, for two minutes.
            for allocator in ("mimalloc", "jemalloc", "system"):
                options = [*SINGLE, "--sessions", "150", "--churn", "1.5"]
                options += ["--warmup", "20", "--measure", "120"]
                measure(bin_dir, out_path, allocator, "soak", options)
        else:
            sys.exit(f"unknown part: {part}")


if __name__ == "__main__":
    main()
