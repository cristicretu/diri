# termbench

Compares diri against other terminal emulators on the same machine, by
replaying identical byte streams into each and timing how long each takes to
*finish parsing* them.

## Why it is built this way

Timing `cat` alone measures how fast a terminal drains the PTY, which a
terminal can win by buffering and parsing later. Every measurement here queues
a cursor-position request (`CSI 6n`) behind the payload and waits for the
answer, so the clock stops only once the terminal has actually processed
everything ahead of it. That is `t_sync`; `t_cat` is reported alongside it for
comparison, and the two diverging is itself informative — it means the terminal
accepted bytes faster than it consumed them.

Terminals are measured one at a time. Two running at once measures the
contention between them.

Runs are short and the machine is not quiet, so every workload runs several
times and the **fastest** run is reported: interference only ever makes a run
slower, so the minimum is the closest estimate of what the terminal costs.
Medians looked stable in early runs and were not — individual passes varied by
up to 2.4x until the payloads were grown to 40 MB.

## Running it

```sh
# One-time: the plain-text workload replays a large public-domain text file.
mkdir -p /tmp/termbench-src && curl -o /tmp/termbench-src/shakespeare.txt <url>

python3 gen_workloads.py /tmp/termbench      # write the byte streams
cargo build --release -p diri-engine --examples --bins

# All three terminals, sequentially:
RUNS=5 ./compare_all.sh
python3 table.py results
```

To measure one terminal, run `suite.sh <name>` *inside* it. For diri there is
no window to run inside, so `examples/runbench` spawns a real held session and
runs the same script there — the same PTY, holder, log and emulator the daemon
uses, with no renderer attached.

## Workloads

Each is a recording of what some real program writes: plain text scroll, dense
truecolor SGR, scattered cursor motion, unbroken lines that force wrapping,
scroll regions with inserts and deletes, wide CJK, decomposed combining marks,
erase-and-redraw churn, alternate-screen switching, synchronized updates
(DECSET 2026), a build-log mix, and a captured DOOM-fire animation.

## The drain regression test

`crates/diri-engine/tests/throughput.rs` measures the same path from inside the
engine and prints its rate on every CI run, but only fails below a floor when
`DIRI_PERF_ASSERT=1` is set. The shared runner builds the whole workspace and
runs its test binaries concurrently on two cores; one commit measured 109 MB/s
in the engine job and 8 MB/s in the workspace job, so a threshold there would
report contention as a regression. Run it on a quiet machine to use it as a
gate:

```sh
DIRI_PERF_ASSERT=1 cargo test --release -p diri-engine --test throughput
```

## Latency

`dsr_latency.py` measures query round trip on an idle terminal.
`latency_under_load.py` measures the same thing while a payload streams, which
is the number that corresponds to typing while a build log scrolls. The gap
between the two is the interesting part: a terminal whose idle latency is
microseconds and whose loaded latency is tens of milliseconds will *feel* slow
exactly when a user is most likely to be interacting with it.

For diri specifically, loaded latency is governed by how far the holder may run
ahead of the daemon that parses its output — see `WRITE_QUEUE_DEPTH` in
`holder/server.rs`. Deepening that queue raises throughput and worsens the
latency tail, one for one. The queue is also why a benchmark payload smaller
than the queue reports a rate the pipeline cannot sustain.
