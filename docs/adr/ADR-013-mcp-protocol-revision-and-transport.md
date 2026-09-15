# ADR-013: MCP target revision and transport (rmcp vs hand-rolled)

| | |
|---|---|
| **Status** | **Proposed** — decision deferred to Phase 2 step 14 by design |
| **Date** | 2026-09-15 |
| **Milestone** | Phase 2 (MASTER_PLAN.md §15 step 14) |

## Context

ARCHITECTURE.md §4.9 and §14 specified two things that verification showed to be stale
and under-justified respectively:

1. The MCP server would implement `initialize`, `tools/list`, `tools/call`.
2. The Rust SDK `rmcp` would be used.

The MCP server is a **thin adapter over an already-working engine** and is scheduled for
Phase 2 step 14 — it is explicitly not the conceptual centre of the product (MCP is a
thin interface over the same core selection and rendering engine; the CLI is primary).
Committing to a protocol revision and an async runtime now, before a single selection
stage exists, would be deciding architecture for a component that cannot yet be tested.

`rmcp` is therefore **not in the workspace manifest** and appears nowhere in
`Cargo.toml`. This ADR records the verified facts so the Phase 2 decision is made on
evidence instead of being re-researched, and states the criteria that will settle it.

## Verified facts (2026-09-15)

**The MCP specification revision is `2026-07-28`, and it removed the handshake.**
Confirmed against the specification's own versioning page, which states the current
protocol version is 2026-07-28, and against the 2026-07-28 changelog, whose major
changes include:

- *"Make MCP stateless: remove the `initialize`/`notifications/initialized` handshake.
  Every request now carries its protocol version and client capabilities in `_meta`."*
- *"Add `server/discover`: servers MUST implement this RPC to advertise their supported
  protocol versions, capabilities, and identity."*
- *"All results now carry a required `resultType` field"* — `"complete"` for ordinary
  results.
- `ping` and `logging/setLevel` removed.

Revision history: `2024-11-05`, `2025-03-26`, `2025-06-18`, `2025-11-25`, `2026-07-28`
(current). stdio remains a fully supported transport; it is not in the deprecated
registry.

**`rmcp` cannot serve stdio without an async runtime.** Verified from its manifest and
source:

- `tokio` is an unconditional, non-optional dependency (`features = ["sync", "macros",
  "rt", "time"]`). It cannot be compiled out.
- Every transport is async. `transport-async-rw` requires `AsyncRead + AsyncWrite`.
- The stdio transport is seven lines: `pub fn stdio() -> (tokio::io::Stdin,
  tokio::io::Stdout) { (tokio::io::stdin(), tokio::io::stdout()) }`.
- There is no blocking or synchronous transport, and no feature combination that avoids
  the runtime.
- A minimal `default-features = false, features = ["server"]` build resolves ~50
  transitive crates, including `schemars`, `uuid` and `pastey`, which exist to support
  macro-driven tool-schema generation.
- Positively: with `default-features = false` the stdio path is genuinely zero-network —
  `mio`, `socket2` and `reqwest` are absent from the lockfile. So the *network* guarantee
  survives; the *runtime* cost does not disappear.

**`rmcp` supports both protocol eras, but does not default to the current revision.**
It defines every revision including `V_2026_07_28`, but `ProtocolVersion::LATEST` is
still `V_2025_11_25`. Era negotiation is the genuinely hard, high-churn part of this
problem and `rmcp` absorbs it.

## Decision

**Defer.** Do not add `rmcp` or any MCP dependency during bootstrap. When the MCP adapter
is built (Phase 2 step 14), it must target the **current revision (2026-07-28)** and
implement `server/discover` plus mandatory `_meta` handling — a handshake-only server is
a forward-compatibility dead end, because a modern client against a legacy handshake
server fails.

The transport decision is settled by these criteria, in order:

1. **If the adapter stays a thin, read-only, 5-tool server** (the current design) **and**
   a spec audit shows a single-era implementation stays under ~500 lines of first-party
   code, **hand-roll it** on `serde` + `serde_json`, staying synchronous and keeping the
   dependency tree trivially auditable for the zero-network guarantee (ADR-010).
2. **If dual-era compatibility, HTTP transport, or auth is required**, adopt `rmcp`,
   pinned as `default-features = false, features = ["server", "transport-io"]`, and add a
   CI assertion that `mio`, `socket2` and `reqwest` are absent from `Cargo.lock`.
3. Either way, CI must assert the zero-network property rather than assume it.

## Consequences

- The bootstrap ships no MCP dependency, no `tokio`, and no async runtime. The CLI stays
  synchronous, which is the shape the other three contracts (stdout discipline, exit
  codes, <50 ms startup per MASTER_PLAN.md §9) are built around.
- ARCHITECTURE.md §14 no longer asserts `rmcp` as a choice; it points here.
- Phase 2 carries a mandatory prerequisite: re-check the then-current MCP revision before
  implementing, since this ADR's protocol facts have a shelf life.
- Deferring costs nothing now and avoids a commitment that would be made without a single
  selection stage to test against.

## Alternatives rejected

- **Adopt `rmcp` during bootstrap** — would pull ~50 crates and an async runtime into a
  synchronous, local-first CLI to serve an adapter scheduled two phases away.
- **Hand-roll now, during bootstrap** — implements a protocol against a skeleton engine;
  the tool bodies would be untestable and the protocol work would need redoing anyway.
- **Target `2025-06-18`** (as the original spec text implied) — two revisions behind and
  removed the handshake; a forward-compatibility dead end.
- **Mark repo content as untrusted via a spec mechanism** — no such mechanism exists.
  `ToolAnnotations` carries only `title`, `readOnlyHint`, `destructiveHint`,
  `idempotentHint`, `openWorldHint`, and the spec states these are hints that clients
  must not rely on. Resource/content annotations carry only `audience`, `priority`,
  `lastModified`. The untrusted-content warning therefore stays a *convention we
  implement in tool descriptions* (SECURITY.md §4.2); it is not a compliance gap.

## What would reopen this

The Phase 2 step 14 implementation itself. This ADR is `Proposed`, not `Accepted`, and
will be superseded by an accepted record naming the chosen transport with the measured
line count or dependency-tree assertion that justified it.
