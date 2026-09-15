# ADR-010: No telemetry, ever by default

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

Usage data would genuinely help prioritize languages, tune weights, and detect failures.
ContextSlice also reads private source code, which makes any data collection a trust
question rather than a metrics question.

## Decision

No telemetry, no update checks, no crash reporting, no phone-home — not opt-out, not
anonymized, not aggregated. The core never opens a network socket, and CI enforces this
with a network-blocked test that fails the build on any socket attempt.

## Consequences

- We lose product analytics permanently. Prioritization comes from the benchmark,
  GitHub issues, and explicit user reports instead.
- The privacy claim is verifiable rather than a promise: no network capability, an
  inspectable SQLite file, and no source text stored at all (only spans and signatures).
- Whenever the MCP adapter lands, its dependency footprint becomes a security-relevant
  decision, because a transitive HTTP client would undermine this guarantee — hence the
  explicit dependency-tree assertion required by ADR-013.

## Alternatives rejected

- **Opt-in telemetry** — the failure mode is a user enabling it once on a work machine
  and forgetting; and maintaining a collection endpoint conflicts with having no service.
- **Anonymized aggregate counts** — still needs egress, still needs a server, still
  cannot be audited by the user.
- **Local-only counters reported by `doctor`** — kept as an option; it collects nothing
  off-machine, so it does not violate this ADR.

## What would reopen this

Nothing. This is a stated product commitment (SECURITY.md §1), and changing it would be
a breaking trust change, not an engineering trade-off.
