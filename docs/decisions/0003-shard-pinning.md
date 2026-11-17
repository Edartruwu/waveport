# Decision 0003: when the threads of the shards are pinned

**Date:** 16 November 2026. **Status:** accepted, and built in phase 2 (milestone 9). **Phase:** 1, after milestone 11.

The [roadmap](../../ROADMAP.md) had the threads of the shards pinned to cores in phase 1. Milestone 11 measured what pinning is worth and did not build it. This record says what that measurement was, why pinning is not built yet, when it is built, and what to build then.

## Decisions

| Question | Decision | Why |
|---|---|---|
| When | With `waveportd`, the daemon of phase 2, together with the setting for the number of shards | The `waveport` command runs one shard and has no setting for more. And what was measured does not show a gain on machines whose cores are alike |
| Default | Off. Whoever runs the process names the processors | Two processes that each chose the first cores for themselves would share them. Only whoever runs the machine knows which cores are whose |
| Platforms | Linux. Asked for anywhere else, it is an error and not a silent nothing | Linux on x86-64 and arm64 is tier 1 (section 17 of the [architecture](../architecture.md)). macOS has no call that binds a thread to a core, only a hint |
| How | The `rustix` crate, for Linux only | Its call is safe, and the workspace forbids `unsafe`. It takes a set of processors and not one, which the other threads of the process need |
| Until then | `taskset` around the command, on a machine whose cores are not alike | It is what was measured, and it needs no code |

## What was measured

Fifty calls on one shard in real time (`benches/tests/load.rs`), with a stand-in for every provider and for the models that listen. The laptop's figures are in section 12 of the architecture and in `perf/baselines`; the runners' are in the logs of the nightly runs.

| Where | A session's share of its thread | Runs of a session over 250 µs |
|---|---|---|
| Laptop (Core Ultra 7 155H) at full speed, kept on one performance core | 0.23% | 3.5 in 10,000 |
| The same, left to the system | 0.24% | 2 in 1,000 |
| The laptop when it was slow, kept on one performance core | 0.48% | 6 in 1,000 |
| The same, left to the system | 0.57 to 0.63% | 3 in 100 |
| GitHub's runners (Xeon 6975P-C, EPYC 9V74 and EPYC 7763), left to the system, twelve runs | 0.20 to 0.27%, by the processor | 3 in 10,000 on the first two, 6 on the third |

The laptop's figures were measured with its balanced power profile. With the profile for performance its one core gives 0.19% and 3 in 10,000, which the performance gates found later (section 12 of the architecture); left to the system it has not been measured that way.

Three things have to be said about it.

- **No thread was pinned.** `cargo xtask perf --cpu` runs the whole measuring process under `taskset`, on one processor. In that test it comes close to pinning the shard, because the thread that drives the test sleeps and the models are stand-ins that run on the shard's own thread. It says nothing about a process whose other threads work: the two of the compute plane, and the journal's.
- **The laptop's cores are not alike:** six performance cores, eight efficiency cores and two low-power ones. Left to the system, a thread is moved among them, and that is what `taskset` took away.
- **On the runners, whose cores are alike, nothing was kept anywhere, and on two of their three processors the long runs were as rare as on the laptop's one core.** On the third and slowest there were twice as many, which is still three times fewer than on the laptop left to the system.

So the gain that was measured is this laptop's. It is not evidence for a server. What pinning is worth where it should matter most, on a machine with every core at work, has not been measured by anybody: section 18 of the architecture already asks for that measurement.

## What to build

- **The setting.** `RuntimeConfig` gains the processors of the shards, one for each. It stops being `Copy`, which the crate may do while it is experimental.
- **Where a thread is bound.** In the thread of the shard itself, before its executor is built (`start_shard`). The thread already tells its maker whether its executor is there, and a processor that could not be had goes back the same way, so that `Runtime::new` fails and says which.
- **Which processors may be asked for.** Those the process is allowed, which the system tells (`sched_getaffinity`). That is how the processors a container was given are respected.
- **The threads a shard starts.** The executor of a shard starts threads for work that blocks, and a new thread inherits the processors of its maker: a name is looked up on such a thread whenever an adapter connects. The builder of the executor has a hook that runs on each of them as it starts (`on_thread_start`; in tokio 1.53 it runs on those threads and not on the shard's own), and there they are given the processors that are no shard's.
- **The other threads of the process**: `waveport-frames`, `waveport-turns`, `waveport-journal`, and those of telemetry. The daemon narrows its main thread to the processors that are no shard's before it starts any of them, and they inherit that.
- **The crate.** `rustix` 1.1 with its `thread` feature: `sched_setaffinity` and `sched_getaffinity` for a thread, with a set of processors. Declared for Linux only, it adds `linux-raw-sys` to the tree; `bitflags` is there already.
- **Not `core_affinity`**, which the density spike used. It binds a thread to one processor and cannot give the other threads a set of them. It brings `num_cpus`, and `winapi` on Windows. On macOS it sets an affinity tag, which is a hint.
- **No choosing by the runtime.** To choose well it would have to know which cores are fast, which are two threads of one core, and what else runs on the machine.

## As it was built

It was built in phase 2 with the daemon, as decided, and differs from the list above in three places.

- **The setting is a plan, and not a list of processors.** `RuntimeConfig` has a `Pinning`: the processor of each shard, and the processors that are left for every other thread. Two lines of the list above cannot both be had as they are written. The processors that may be asked for are the ones the thread that asks may run on, and the daemon keeps its first thread off the shards' processors before the runtime is built: by then that thread may no longer run on the processors of the shards, and asking it says so. So the question is asked once (`Pinning::here`), while the thread may still run wherever the process may, and the answer is handed on. A `Pinning` can be made in no other way, and on a system that is not Linux in none.
- **The other threads need a processor.** Naming every processor the process may run on for a shard is refused, because the threads that are no shard's would have none.
- **A shard tries the processors of the other threads first.** The hook that sends a shard's blocking threads to the processors that are no shard's cannot tell anybody that it failed. So the thread of a shard is kept on those processors for a moment before it is kept on its own, where a failure stops the runtime from starting and says why.

In the daemon the setting is `processors`, one for each of its `shards`. A daemon that is told it and cannot do it does not start: on a system that is not Linux, with a processor the process may not run on, or with none left over. `waveportd --check` reads the file and does not ask the machine. A set of processors has room for 1,024, as the system's own call has by default; a machine with more is answered with an error.

Linux cuts the name of a thread after fifteen letters, so the threads of the shards are all called `waveport-shard-` in `ps` and `top`. They are told apart by their processors, which are in the order of the shards.

Nothing was measured with it. The measurements the next section asks for are still to be made, and pinning is still off unless the processors are named.

## What has to be measured before it is on by default anywhere

- **A server with every core at work**, and more than one shard.
- **Time on the processor, and not by the clock.** A shard times each run of a session by the clock, so a run that was interrupted counts as a long one. The spike read the time each thread had on its processor from the scheduler; a measurement of pinning should do the same.
- **A process whose other threads work**: the models that listen, on the threads of the compute plane, and the journal.

## What is not answered

- **Two threads of one core.** The spike left the second thread of a shard's core idle. A processor of a cloud machine is usually one such thread, and whether a shard should have the whole core is not measured.
- **Machines with more than one memory node.**
- **Windows**, which has such a call. It can be added in the same shape if somebody runs a server there.
