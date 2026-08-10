# ADR-003: Use Config-Driven Providers and Models

> **注意**：当前 config v2 中，客户端可见模型名已统一为 `[models.<id>]`，即 **model id**。本文档中的 `frontend_id` 已弃用。

## Status

Accepted; amended by [ADR-007](007-simple-model-provider-configuration.md) for config v2 naming and provider-specific model bindings

## Date

2026-04-30

## Context

llm-proxy needs to support multiple upstream providers and expose frontend model names that may differ from provider model IDs. It also needs provider fallback, protocol conversion, and local model list responses.

A model used by a client has two identities:

- `frontend_id`: the model name clients send to llm-proxy.
- `model_id`: the model name llm-proxy forwards to the upstream provider.

## Decision

Use the configuration file as the source of truth for providers and models.

Each model declares:

- frontend ID
- upstream model ID
- frontend protocol type
- ordered provider list for fallback
- optional Codex metadata such as context window and max output tokens

The proxy's `/v1/models` endpoint returns configured models locally rather than forwarding the request to any provider.

## Alternatives Considered

### Hard-code provider and model templates only

Pros:

- Simple initial setup.
- Less configuration for common providers.

Cons:

- Users cannot customize provider order, model aliases, or provider endpoints cleanly.
- Hard-coded model lists become stale quickly.
- Fallback policy becomes tied to code changes.

Rejected because provider and model configuration is core user-controlled behavior.

### Forward `/v1/models` to upstream providers

Pros:

- Always reflects provider-side model availability.
- Less local model metadata to manage.

Cons:

- Does not work cleanly with multiple providers and fallback.
- Exposes upstream provider differences to clients.
- Cannot represent local frontend aliases and protocol-specific model entries.
- Makes Codex model catalog generation less deterministic.

Rejected because llm-proxy's configured frontend model list is the user-facing contract.

### Use only provider model IDs as frontend IDs

Pros:

- Less mapping logic.
- Easier to trace requests.

Cons:

- Cannot present stable client-facing aliases.
- Cannot expose the same upstream model through multiple frontend protocol types.
- Makes provider migration visible to clients.

Rejected because aliasing and frontend/proxy decoupling are explicit project requirements.

## Consequences

- Configuration becomes the authoritative model/provider injection point.
- `llm-proxy init` can seed templates, but users can customize them.
- The proxy can fallback across providers while preserving a stable frontend model ID.
- Spec and defaults must stay synchronized with tests.
