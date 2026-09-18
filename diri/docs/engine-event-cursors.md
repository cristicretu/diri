# Engine event cursor scope

`HelloResult.engineInstanceId` identifies one Engine control/event lifetime.
The Engine generates a random 128-bit value when constructing its control
server. Every Hello from that server returns the same value; a new server gets
a new value even if its executable and PID match a previous Engine.

The Rust client resumes from its received event sequence only after verifying
the same Engine instance on reconnect. A new instance resets the cursor before
`events.subscribe`. An older Engine omitting this optional field remains usable,
but subscriptions omit `sinceSeq` because its sequence lifetime cannot be proven.

Hello validation and cursor acceptance share a connection generation. Old socket
readers and delayed Hello/subscription responses cannot change a newer cursor.
Concurrent subscription callers serialize through the actual acknowledgement.
A changed or invalid identity during a heartbeat or explicit `hello()` rejects
further events for that connection and wakes the reconnect loop.

This change does not recover application state after an event gap. Replay is
still bounded, and existing `events.dropped` markers still require a consumer
resynchronization contract. Event broadcast entries already delivered to an
application are not tagged or rolled back by this cursor guard.
