#!/usr/bin/env python3
"""Turns the spike's raw output into the tables of the decision record.

    summarise.py <probe.json> <matrix.jsonl> [<combined.json>]

Prints Markdown tables. With a third argument it also writes the probe and
every run into one JSON file, which is what `perf/spike/` keeps.
"""

import json
import statistics
import sys


def load(probe_path, matrix_path):
    with open(probe_path, encoding="utf-8") as handle:
        probe = json.load(handle)
    runs = []
    with open(matrix_path, encoding="utf-8") as handle:
        for line in handle:
            if line.strip():
                runs.append(json.loads(line))
    # Only what the driver recorded: it adds the attempt number.
    return probe, [run for run in runs if "attempt" in run]


def quality(run):
    """`ok`, or why the driver could not get a clean run."""
    if run.get("clean", True):
        return "ok"
    reasons = []
    if not run.get("quiet_start", True):
        reasons.append("busy at start")
    if run.get("core_slowed"):
        reasons.append("core slowed")
    if run.get("other_cores", 0) > 1.0:
        reasons.append(f"other load {run['other_cores']} cores")
    return ", ".join(reasons) or "suspect"


def throttled(run):
    return quality(run) != "ok"


def label(run):
    return run["label"]


def row(cells):
    return "| " + " | ".join(str(cell) for cell in cells) + " |"


def table(header, rows):
    print(row(header))
    print(row(["---"] * len(header)))
    for cells in rows:
        print(row(cells))
    print()


def density_row(run):
    cost = run["cost_pct_of_core_per_session"]
    return [
        run["sessions"],
        f"{run['shard_busy_pct'][0]:.1f}",
        f"{sum(run['batcher_busy_pct']):.1f}",
        f"{sum(run['turn_busy_pct']):.1f}",
        f"{cost['shard']:.3f}",
        f"{cost['vad']:.3f}",
        f"{cost['turn']:.3f}",
        f"{cost['total']:.3f}",
        f"{run['tick_late_ms']['p99']:.2f}",
        f"{run['ingress_delay_ms']['p99']:.2f}",
        f"{run['vad_delivered_ms']['p99']:.1f}",
        run["vad_dropped"],
        f"{run['turn_request_to_result_ms']['p99']:.0f}",
        quality(run),
    ]


DENSITY_HEADER = [
    "Sessions",
    "Shard busy %",
    "Batcher busy %",
    "Turn threads busy %",
    "Shard cost",
    "Voice-activity cost",
    "End-of-turn cost",
    "Total cost",
    "Tick late p99 ms",
    "Ingress delay p99 ms",
    "Voice-activity result p99 ms",
    "Frames dropped",
    "End-of-turn result p99 ms",
    "Run quality",
]


def main():
    probe, runs = load(sys.argv[1], sys.argv[2])

    print("## Probe: voice activity\n")
    rows = []
    for entry in probe["vad"]:
        if not entry.get("runs"):
            rows.append([entry["backend"], entry["model"], "does not load", "", "", "", ""])
            continue
        by_batch = {b["batch"]: b for b in entry["batches"]}
        rows.append(
            [
                entry["backend"],
                entry["model"],
                f"{entry['max_diff_vs_reference']:.1e}",
                f"{by_batch[1]['per_frame_us_p50']:.1f}",
                f"{by_batch[8]['per_frame_us_p50']:.1f}",
                f"{by_batch[64]['per_frame_us_p50']:.1f}",
                f"{max(b['batched_vs_alone_max_diff'] for b in entry['batches']):.1e}",
            ]
        )
    table(
        [
            "Backend",
            "Model file",
            "Max difference from reference",
            "us per frame, batch 1",
            "batch 8",
            "batch 64",
            "Batched against alone",
        ],
        rows,
    )

    print("## Probe: end of turn\n")
    rows = []
    for entry in probe["turn"]:
        if not entry.get("runs"):
            rows.append([entry["backend"], "does not load", "", "", ""])
            continue
        rows.append(
            [
                entry["backend"],
                f"{entry['inference_ms']['p50']:.1f}",
                f"{entry['inference_ms']['p99']:.1f}",
                f"{entry['max_diff_vs_reference']:.2f}",
                f"{entry['mean_diff_vs_reference']:.2f}",
            ]
        )
    table(
        ["Backend", "Inference p50 ms", "p99 ms", "Max difference", "Mean difference"],
        rows,
    )
    print(f"Feature extraction: {probe['mel_ms']['p50']:.1f} ms per window.\n")

    print("## Ramp\n")
    table(DENSITY_HEADER, [density_row(r) for r in runs if label(r) == "ramp"])

    print("## Knobs at 100 sessions\n")
    knobs = [r for r in runs if label(r) == "ramp" and r["sessions"] == 100]
    knobs += [
        r
        for r in runs
        if label(r) in ("complexity-5", "greedy", "energy-gate", "complexity-5-energy-gate")
    ]
    rows = []
    for run in knobs:
        cost = run["cost_pct_of_core_per_session"]
        name = "defaults" if label(run) == "ramp" else label(run)
        rows.append(
            [
                name,
                f"{cost['shard']:.3f}",
                f"{cost['vad']:.3f}",
                f"{cost['turn']:.3f}",
                f"{cost['total']:.3f}",
                f"{run['encode_us']['p50']:.0f}",
                f"{run['mean_batch']:.1f}",
                f"{run['vad_push_to_result_ms']['p50']:.2f}",
                f"{run['vad_push_to_result_ms']['p99']:.2f}",
                quality(run),
            ]
        )
    table(
        [
            "Setting",
            "Shard cost",
            "Voice-activity cost",
            "End-of-turn cost",
            "Total",
            "Encode p50 us",
            "Mean batch",
            "Frame to result p50 ms",
            "p99 ms",
            "Run quality",
        ],
        rows,
    )

    print("## Allocators at 100 sessions (two runs each)\n")
    rows = []
    for allocator in ("mimalloc", "jemalloc", "system"):
        group = [
            r for r in runs if label(r).startswith("alloc-") and r["allocator"] == allocator
        ]
        if not group:
            continue
        shard = [r["cost_pct_of_core_per_session"]["shard"] for r in group]
        memory = [
            (r["rss_bytes"]["warm"] - r["rss_bytes"]["base"]) / r["sessions"] / 1e3
            for r in group
        ]
        rows.append(
            [
                allocator,
                " / ".join(f"{v:.3f}" for v in shard),
                f"{statistics.median(shard):.3f}",
                f"{statistics.median(r['provider_us']['p50'] for r in group):.1f}",
                f"{statistics.median(r['provider_us']['p99'] for r in group):.1f}",
                f"{statistics.median(r['session_start_initial_us']['p99'] for r in group):.0f}",
                f"{statistics.median(memory):.0f}",
                f"{statistics.median(r['rss_bytes']['end'] for r in group) / 1e6:.0f}",
                sum(throttled(r) for r in group),
            ]
        )
    table(
        [
            "Allocator",
            "Shard cost per session, each run",
            "Median",
            "Provider message p50 us",
            "p99 us",
            "Session start p99 us",
            "KB per session",
            "RSS at end MB",
            "Suspect runs",
        ],
        rows,
    )

    print("## Churn soak: 150 sessions, each replaced about every 40 s, 2 minutes\n")
    rows = []
    for run in [r for r in runs if label(r) == "soak"]:
        rss = run["rss_bytes"]
        start = run["session_start_churn_us"]
        rows.append(
            [
                run["allocator"],
                f"{rss['base'] / 1e6:.0f}",
                f"{rss['warm'] / 1e6:.0f}",
                f"{rss['max'] / 1e6:.0f}",
                f"{rss['end'] / 1e6:.0f}",
                f"{(rss['end'] - rss['base']) / run['sessions'] / 1e3:.0f}",
                start["count"],
                f"{start['p50']:.0f}",
                f"{start['p99']:.0f}",
                f"{start['max']:.0f}",
                f"{run['cost_pct_of_core_per_session']['shard']:.3f}",
                quality(run),
            ]
        )
    table(
        [
            "Allocator",
            "RSS before sessions MB",
            "After warm-up MB",
            "Peak MB",
            "At end MB",
            "KB per session at end",
            "Sessions started",
            "Start p50 us",
            "p99 us",
            "max us",
            "Shard cost",
            "Run quality",
        ],
        rows,
    )

    if len(sys.argv) > 3:
        with open(sys.argv[3], "w", encoding="utf-8") as handle:
            json.dump({"probe": probe, "runs": runs}, handle, indent=1)
            handle.write("\n")


if __name__ == "__main__":
    main()
