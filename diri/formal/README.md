# Ordered local hook delivery

Run `PATH="$HOME/.elan/bin:$PATH" lake build` in this directory. Lean 4.36 checks the model using the standard library.

| Lean | Rust counterpart |
| --- | --- |
| `issued`, `Ticket` | For one session in one Engine lifetime, `HookOrder::reserve` links each sequence to the previous reservation. Other sessions can create gaps in the numeric IDs. A ticket describes its eventual validated commit or cancellation. |
| `Run.receive` | `ControlServer::serve_reserved_hook` reads the first commit on the connection and checks its sequence and intent. EOF or an invalid frame resolves the reservation as a cancellation. Readiness may occur in any order. Repeated receipts cannot create reservations. |
| `Run.finish` | `Reservation::finish` waits for the predecessor before handing the event to `ControlServer::apply_hook_report`. `Reservation::drop` releases the successor after handoff or cancellation. One connection consumes at most one frame. |
| `applyTicket` | A cancelled slot leaves state unchanged. A validated report enters `HookQueue::submit`, whose inline and queued paths preserve submission order, then `Registry::apply_hook_report` and `StatusReducer::reduce`. The model quantifies over the transition function, including stateful hooks that leave the parent status unchanged. |

`permutation_drains_in_order` shows that any permutation of fully delivered reservations can drain. `final_in_order` covers all completed interleavings of receipt and handoff. `at_most_once` covers incomplete runs and repeated receipts as well. The checked increment in `reserve` supplies the unique-sequence premise. The proof compares the same event values and clock inputs. It models one session's hook stream and assumes handlers able to run eventually do. Process crashes, unrelated terminal signals, and wall-clock-dependent changes to other Engine state are outside the model.

The Rust test `shuffled_commits_match_in_order_status` uses the production reservation, Claude parser and status reducer with fixed timestamps.
