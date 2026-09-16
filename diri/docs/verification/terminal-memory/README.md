# Deferred alternate-screen allocation

`report.png` captures `report.html`, a report of actual local allocation measurements,
not application UI or a competitor comparison. An 80×24 core measured 100,643 →
53,795 requested heap bytes (46.5% lower); after first publication, 132,515 → 85,667.
Seven samples each, macOS arm64, optimized profile. The fleet harness checks final
output and verifies all core allocations are freed.

After 10,000 numbered 72-column ASCII lines, memory changed from 6,022,819 to
5,975,971 bytes. Retention remains 2,184 history rows and 24 visible rows. This
change reduces unused-screen cost; it does not optimize history representation.
First alternate entry allocates at the current dimensions; subsequent entries
reuse the existing grid. No controller, PTY, history budget, or wire format change.

```sh
cargo bench -p diri-terminal-state --bench terminal_parity -- --empty-gate
cargo bench -p diri-terminal-state --bench terminal_parity
cargo test -p diri-terminal-state
```

The empty gate is a requested-heap engineering budget of 68 KiB, not a total-process
memory target. Before the change it failed with 100,643 bytes; afterward it passes
with 53,795. Existing parser tests and added screen-switch/reset/resize coverage
check terminal semantics. The vendored parser suite also passed 132 tests and
one doctest using Diri's VTE patch.
