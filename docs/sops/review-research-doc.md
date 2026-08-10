# SOP: Review Research Documents

Status: active
Last updated: 2026-08-10 (removed tool binding, made tool-agnostic)
Scope: review gate for external research documents

## 1. Purpose

Every newly written or refreshed external research document must be reviewed before it is considered accepted. This SOP defines the review process and acceptance criteria.

Use this for:

- provider platform research
- LLM app/client research
- reference project research

## 2. Review Process

1. Identify the document to review and its target (platform/app/project name)
2. Run the review against the acceptance criteria in §4
3. If review returns `BLOCKED`, fix the document and rerun
4. Record the final review date/result in the reviewed document

## 3. Placeholders

Fill these before review:

| Placeholder | Example |
|---|---|
| `<DOC_PATH>` | path to the research document |
| `<DOCUMENT_CLASS>` | `provider-platform`, `llm-app-client`, or `reference-project` |
| `<TARGET_NAME>` | `Kimi / Moonshot`, `pi`, `CliProxyAPI` |

## 4. Review Criteria

Evaluate the document against these acceptance criteria:

1. Has `Last refreshed: YYYY-MM-DD` near the top.
2. Has a `References` section with official docs/blog/changelog/source links or local source paths.
3. Separates facts from implementation plans/recommendations.
4. Every claim that can affect catalog, launch, auth, endpoint compat, protocol conversion, reasoning, fallback, cooldown, or model features has evidence or is marked `unknown`.
5. Stale facts are removed or explicitly marked historical/legacy.
6. No product/platform/app behavior is inferred from memory or brand names.
7. The granularity is correct:
   - provider document: one platform, with product-level table for model service products;
   - app/client document: one supported app/client;
   - reference-project document: one target project.
8. The document is actionable enough to derive implementation tests or catalog/config updates.

### Additional provider-platform checks, if applicable:

- Product table includes current/default, current-advanced, legacy-existing-users, deprecated, or unknown status where applicable.
- Product eligibility is explicit, especially legacy-vs-new-user plans.
- Native protocol endpoints and derived endpoint implications are documented.
- Endpoint compat evidence covers `supports_developer_role`, `supports_reasoning_effort`, `thinking_format`, `requires_reasoning_content_on_assistant_messages`, and `max_tokens_field` when relevant.
- Model IDs, context window, max output, tools, image, document/PDF, and reasoning levels/defaults are documented or marked unknown.
- Auth, quota/billing, rate-limit, model-unavailable, and retry/error behavior are documented or marked unknown.

### Additional app/client checks, if applicable:

- Config path/schema and OS differences are documented.
- Managed-region/managed-entry preservation rules are precise.
- Required frontend protocol/wire API is explicit.
- Base URL, API key, provider/model schema, model metadata, and capability metadata are documented.
- Observed app version/commit/build is recorded.

### Additional reference-project checks, if applicable:

- Repository, commit/release, and relevant source files are cited.
- Config/provider/model abstraction, conversion, fallback/routing, and launch/config generation relevance are summarized.
- Lessons are categorized as `adopt`, `avoid`, or `monitor`.
- The doc does not use the reference project alone to justify provider catalog defaults.

## 5. Output Format

- **Verdict**: PASS or BLOCKED.
- **Blocking issues**: numbered list, each with file section and required fix. Use `None` if PASS.
- **Non-blocking suggestions**: concise bullets.
- **Evidence gaps**: claims that need better references or should be marked unknown.
- **Downstream updates needed**: catalog/design/parity/ADR/tests, if any.

**Important**: Be strict. If evidence is missing for a claim that affects implementation behavior, return BLOCKED.
