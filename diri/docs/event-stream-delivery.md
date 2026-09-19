# Event stream delivery

`events.subscribe` assigns sequence numbers within one Engine lifetime. Concurrent publishers enqueue in that sequence order. Subscription replay and the live tail use the same publication lock; no socket writes occur under that lock.

Each subscriber has a bounded data queue. If a slow consumer falls behind, the oldest matching events are evicted. The next read returns an `events.dropped` marker before the surviving events, including when the producer has stopped. The marker uses sequence zero and does not occupy a data slot.

The existing marker payload has `dropped`, `fromSeq`, and `toSeq`. A consumer must treat this as a coverage gap and refresh the state it needs. It should not advance its replay cursor to the marker's zero sequence.

If `sinceSeq` is older than the retained replay ring, the first read likewise reports the unavailable range. That range covers global events and can include events outside the subscription filter. It does not prove that a particular matching event was lost. Filters cannot suppress gap markers.

This stream is bounded, not a durable journal. Sequence numbers are local to one Engine lifetime; clients must resynchronize after Engine replacement. This change does not add new terminal event kinds or claim exactly-once delivery across reconnects.

`HelloResult.engineInstanceId` lets a client tell one Engine lifetime from the next; see [engine-event-cursors.md](engine-event-cursors.md) for how the Rust client scopes its replay cursor to it.

The desktop treats both an `events.dropped` marker and a lag on its local broadcast as a coverage gap: it requests one authoritative `session.list` snapshot (requests coalesce while one is in flight), replaces sessions and projects from it, and refreshes the workspace catalog through its own revision gate. Lost notification bodies are not reconstructed; only current state is repaired.
