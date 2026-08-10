# ADR-004: Use Responses API for Codex Integration

## Status

Accepted

## Date

2026-04-30

## Context

Codex can be configured with a model provider and wire API. llm-proxy's main target flow is to let Codex use configured upstream providers through a local proxy.

Codex's configurable provider integration uses the Responses API wire format. For llm-proxy's supported Codex integration, Chat Completions is not an alternative frontend wire API; Codex requires Responses-compatible local endpoints. Many upstream providers still expose Chat Completions or Anthropic-compatible endpoints rather than native Responses API, so llm-proxy must convert internally.

## Decision

Configure Codex to use llm-proxy through the Responses API wire format.

`llm-proxy launch codex` generates Codex model metadata and configures the proxy provider with:

- `base_url` pointing to llm-proxy's `/v1` base URL
- `wire_api = "responses"`
- model catalog entries derived from configured `openai-responses` models

When the selected upstream provider is not Responses-native, llm-proxy performs protocol conversion internally.

## Alternatives Considered

### Use Chat Completions wire API for Codex

Rejected because Codex does not support Chat Completions as the llm-proxy frontend wire API. Chat Completions may still be an upstream provider protocol behind llm-proxy, but Codex-facing local endpoints must be Responses-compatible.

### Configure each provider directly in Codex

Pros:

- Avoids running a local proxy for providers Codex already supports.
- Less proxy responsibility.

Cons:

- Codex configuration becomes provider-specific and harder to maintain.
- No central fallback policy.
- No unified frontend model aliases.
- Provider-specific protocol differences leak into Codex config.

Rejected because llm-proxy is intended to provide a stable local model/provider abstraction.

### Expose only provider-native protocol to Codex

Pros:

- Minimal conversion.

Cons:

- Fails because Codex expects Responses behavior on the frontend, regardless of whether the upstream provider exposes Chat or Anthropic.
- Prevents a consistent Codex launch workflow.

Rejected because it does not meet the `init → launch codex → codex` workflow goal.

## Consequences

- Responses ↔ Chat conversion is a critical path and must be heavily tested.
- Codex-specific quirks, such as shell call format and `call_id`, must be preserved.
- Provider-native protocol differences are hidden from Codex.
- The README can stay focused on the simple Codex workflow.
