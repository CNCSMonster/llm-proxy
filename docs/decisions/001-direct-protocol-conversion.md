# ADR-001: Use Direct Protocol-to-Protocol Conversion

## Status

Accepted

## Date

2026-04-30

## Context

llm-proxy needs to translate between multiple LLM API protocols:

- OpenAI Chat Completions
- OpenAI Responses API
- Anthropic Messages API

These protocols differ in message shape, tool definitions, tool call outputs, streaming events, reasoning fields, and provider-specific extensions. A common alternative is to introduce a unified intermediate representation and convert all protocols through it.

## Decision

Use direct protocol-to-protocol conversion for each supported direction.

Each conversion direction is implemented explicitly, for example:

- Responses → OpenAI Chat
- OpenAI Chat → Responses
- OpenAI Chat → Anthropic
- Anthropic → OpenAI Chat

## Alternatives Considered

### Unified intermediate representation

Pros:

- Fewer conceptual conversion paths.
- A single internal model can look cleaner.
- New provider code may appear simpler at first.

Cons:

- Tool calls, shell calls, reasoning content, and streaming events do not map cleanly to one shared shape.
- Protocol-specific fields would either be lost or stored as escape hatches.
- Streaming conversion would still need protocol-specific logic.
- Adding a new protocol could require changing the shared representation and all existing conversions.

Rejected because llm-proxy prioritizes preserving protocol-specific information over reducing the number of conversion functions.

### Provider-specific passthrough only

Pros:

- Simplest implementation.
- Minimal conversion risk.

Cons:

- Does not solve the core use case: clients such as Codex need a specific frontend protocol while providers may expose another protocol.
- Prevents provider fallback across protocol types.

Rejected because cross-protocol fallback and client compatibility are core project goals.

## Consequences

- Conversion code is more explicit and may have more functions.
- Each conversion path can preserve protocol-specific behavior and edge cases.
- Tests must cover each supported conversion direction.
- When adding a protocol, implement only the required direct conversions instead of modifying a shared global model.
