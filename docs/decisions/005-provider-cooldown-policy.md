# ADR-005: Provider cooldown is model-scoped priority lowering

## Status

Accepted

## Date

2026-05-04

## Context

`llm-proxy` uses model-driven provider chains. When the first provider in a chain becomes temporarily unable to serve a model, subsequent requests should naturally prefer later providers instead of repeatedly paying the latency cost of the failing provider.

At the same time, cooldown must not make the system unavailable when every provider is cooling down. Users also need to observe why a provider-model pair is cooling down, fix the external cause (for example rate limit, billing, or model availability), and explicitly clear the cooldown early.

## Decision

Use provider cooldown as a short-lived priority-lowering mechanism scoped to `(frontend_id, provider)`.

Key decisions:

- Cooldown is scoped per frontend model and provider, not globally per provider.
- Cooldown is based on `Cooldown Category`, not localized reason strings.
- Requests try active providers first, then probe cooling providers if needed.
- Cooldown state is persisted to `~/.cache/llm-proxy/cooldowns.json` using absolute `expires_at` timestamps.
- CLI exposes `llm-proxy cooldown list` and `llm-proxy cooldown clear`.
- Configuration exposes only four intuitive category durations: `network_seconds`, `server_error_seconds`, `rate_limit_seconds`, `model_unavailable_seconds`.
- The old single `fallback.cooldown_seconds` configuration is not retained.

## Alternatives Considered

### Global provider cooldown

Rejected. A provider may be unavailable for one model while still serving another model correctly. Global cooldown would incorrectly affect unrelated model routes.

### Skip cooling providers completely

Rejected. If all providers are cooling down, requests would fail without probing providers that may already have recovered. Cooling providers are therefore lowered in priority, not made unreachable.

### Single cooldown duration

Rejected. Different failure categories have different recovery windows. Network failures should cool down briefly, while model availability failures can reasonably cool down longer.

### Expose persistence controls in config

Rejected for initial implementation. State persistence is part of cooldown semantics and should be enabled by default at a fixed cache path. Exposing `persist`, `state_path`, `min_seconds`, or `max_seconds` would add configuration surface without solving the core user workflow.

## Consequences

- The routing layer becomes more robust when the first provider in a chain is temporarily unavailable.
- Users can inspect cooldown state offline via the persisted state file.
- Users can clear a specific provider-model cooldown after external remediation.
- Implementation must keep state file writes atomic and reload external state changes before cooldown checks.
- `status` remains a health overview; detailed cooldown inspection and mutation live under the `cooldown` subcommand.
