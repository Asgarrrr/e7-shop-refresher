# Initial stream anchor evidence

Repository revision examined: `24fd7fd38883faf70d6559e967b23b296a9c0fbf`.

Decision: ADD_BOUNDED_WINDOW

Invariant source: operator risk decision on 2026-08-17. No authoritative raw-stream server decoder contract is present; the operator explicitly selected the conservative bounded-window behavior rather than accepting known prefix loss.

Payload committed: no

## Client characterization

The deterministic test burst has one server-to-client half-stream and three
contiguous, synthetic segments. Only relative metadata is recorded here:

| Segment | Relative sequence offset | Payload length |
|---|---:|---:|
| S0 | 0 | 2 bytes |
| S1 | 2 | 2 bytes |
| S2 | 4 | 2 bytes |

No capture timestamp exists on `Segment`. Relative arrival time below therefore
means deterministic arrival rank `r0`, `r1`, and `r2`, not an observed duration.

| Permutation (`r0`, `r1`, `r2`) | Forwarded relative span | Forwarded length | Decoder result |
|---|---:|---:|---|
| S0, S1, S2 | `[0, 6)` | 6 bytes | Not evaluated: decoder unavailable |
| S0, S2, S1 | `[0, 6)` | 6 bytes | Not evaluated: decoder unavailable |
| S1, S0, S2 | `[2, 6)` | 4 bytes | Not evaluated: decoder unavailable |
| S1, S2, S0 | `[2, 6)` | 4 bytes | Not evaluated: decoder unavailable |
| S2, S0, S1 | `[4, 6)` | 2 bytes | Not evaluated: decoder unavailable |
| S2, S1, S0 | `[4, 6)` | 2 bytes | Not evaluated: decoder unavailable |

`initial_anchor_all_six_permutations_keep_the_immediate_suffix` records the
matrix. Additional tests establish that sequence wrap uses the same suffix
rule, overlap/retransmission does not replay an earlier prefix, and flow and
direction have independent anchors.

At the application boundary,
`initial_anchor_off_to_on_enqueues_resync_before_triggering_segment` records
`Resync` before the segment that observes the off-to-on gate transition.
The pre-change characterization recorded immediate forwarding. The implemented
`initial_anchor_first_post_resync_segment_waits_once_then_forwards` test now
records the one-shot delay, while the six-permutation application test proves
that every order produces one `ABCDEF` batch.

## Selected limits and behavior

- Window: **10 ms**, measured by Tokio monotonic time from the first segment
  allowed by the direction filter after `Resync`. Filtered segments neither
  start the timer nor consume a budget.
- Rationale: no predecessor-lag measurements are available, so the operator
  authorized the most conservative value inside the pre-agreed 2–10 ms range.
  This is a fixed hard cap, not a claim that observed lag was 9 ms.
- Payload budget: **256 KiB globally** for the one post-resync burst.
- Segment budget: **128 segments globally** for that burst.
- Exact byte or segment limit: include the segment that reaches the limit and
  flush immediately.
- Overflow: if admitting the next segment would exceed either limit, flush the
  existing burst first and process the new segment in steady state. No segment
  is dropped to enforce a budget.
- Other flushes: timer expiry and clean input close flush pending bytes. A
  closed downstream is detected without waiting for the timer; the final send
  is attempted and teardown proceeds when the receiver is already unavailable.
- A newer `Resync` discards the uncommitted burst, clears reassembly, and rearms
  the one-shot state.
- After the first flush, delivery is immediate until the next `Resync`.
- A SYN is never delayed. Any pending burst is committed first, then the SYN is
  passed immediately to `Reassembler`, whose plan-008 incarnation reset remains
  authoritative. New flows appearing while watching are expected to be
  anchored by their captured SYN, which is why the post-resync window is global
  rather than continuously maintained per half-stream.

Within the burst, ordering is performed independently for each
`(FlowKey, Direction)`. The original inter-half-stream slots are retained and
the reordered segments are then fed to `Reassembler`, preserving its overlap,
deduplication, gap, wrap, eviction, and SYN logic. Wrap comparisons rely on the
TCP invariant that a valid outstanding sequence window is smaller than `2^31`;
the 256-KiB cap bounds memory but does not itself bound arbitrary sequence gaps.

## Evidence search

- The only tracked protocol-named candidate, `src/uplink/protocol.rs`, defines
  structured messages returned to the client. It neither contains nor tests
  the server decoder for the raw forwarded stream.
- `src/uplink/websocket.rs` says the server resynchronizes after reconnect.
  This is a client implementation assumption, not an authoritative decoder
  contract, and it has no server revision or decoder replay attached.
- The tracked repository contains no server decoder, representative decoder
  fixture, ingestion protocol specification, or server-side integration test.

The missing proof remains exact: a server decoder source/test at a named revision,
an authoritative ingestion protocol document, or a named maintainer
confirmation must state whether decoding may start at an arbitrary byte
boundary. A sanitized representative input and expected decoded shop result
must eventually be replayed through that same decoder in earliest-first and
later-first order. If the prefix is required, the replay must also show that
restoring it makes the later-first input decode to the same result. None of
those artifacts is present, so decoder success or failure is still not claimed.
This residual uncertainty is why the decision is explicitly an operator risk
choice and the added cost is bounded.

## Consequence

The client pays at most 10 ms once per off-to-on epoch to recover predecessors
that arrive in the initial burst. It never turns this into continuous packet
buffering. Predecessors arriving after the deadline or budgets remain outside
the recovery guarantee. If authoritative decoder evidence later proves
arbitrary-boundary synchronization, this decision can be revisited and the
one-shot latency removed.
