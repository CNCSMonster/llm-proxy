//! Proxy submodule extracted from `proxy::mod`.

use super::*;
use crate::config::ModelConfig;
use axum::extract::FromRequest;

/// JSON request payload paired with the exact bytes it was parsed from.
///
/// Content-type validation and parsing are delegated to axum's `Json`
/// extractor, so rejection status codes and messages stay identical to the
/// plain `Json` extractor; the raw bytes are retained for request
/// fingerprinting (hashing the wire bytes avoids re-serializing the parsed
/// value once per request).
pub(super) struct JsonWithBytes {
    pub value: Value,
    pub bytes: axum::body::Bytes,
}

impl JsonWithBytes {
    /// Build extractor output from a parsed value directly, mirroring what a
    /// client would have sent (the value re-serialized to compact JSON).
    #[cfg(test)]
    pub(super) fn from_value(value: Value) -> Self {
        let bytes = axum::body::Bytes::from(serde_json::to_vec(&value).unwrap_or_default());
        Self { value, bytes }
    }
}

impl<S> FromRequest<S> for JsonWithBytes
where
    S: Send + Sync,
{
    type Rejection = axum::extract::rejection::JsonRejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        // Content-type gate first, in the same order plain `Json` applies it:
        // probe with an empty body — `Json` validates headers before touching
        // the body, so an empty payload can only produce either the
        // content-type rejection (propagate) or a JSON syntax error (content
        // type accepted, continue).
        let mut probe = Request::new(axum::body::Body::empty());
        *probe.headers_mut() = req.headers().clone();
        match Json::<Value>::from_request(probe, state).await {
            // Unreachable in practice: an empty body is never a JSON value.
            Ok(_) => {}
            Err(err @ axum::extract::rejection::JsonRejection::MissingJsonContentType(_)) => {
                return Err(err);
            }
            // Syntax error on the empty probe means the content type passed.
            Err(_) => {}
        }
        let bytes = <axum::body::Bytes as FromRequest<S>>::from_request(req, state).await?;
        let value = Json::<Value>::from_bytes(&bytes)?.0;
        Ok(Self { value, bytes })
    }
}

/// Resolve a routing model hop.
///
/// Returns `Ok(Some(original_id))` when the request was rewritten to a target,
/// `Ok(None)` when the requested model is not a routing model (nothing changes),
/// and an error response when the hop cannot serve this request.
///
/// Routing is a single hop: validation guarantees a hop's target is a regular
/// (executable) model, so there is no chain to walk. `body["model"]` is rewritten
/// only after the hop resolves, so a failure leaves the request as sent.
pub(super) fn resolve_routing(
    state: &AppState,
    protocol: Protocol,
    body: &mut Value,
    err: fn(StatusCode, &str) -> Response,
) -> Result<Option<String>, Box<Response>> {
    let Some(original_model_id) = body.get("model").and_then(Value::as_str).map(String::from)
    else {
        return Ok(None);
    };

    // Clone the hop so the config borrow ends before the (borrowing) helpers run.
    let hop = {
        let cfg = state.config();
        match cfg.models.get(&original_model_id) {
            Some(m) if m.is_routing_model() => m.clone(),
            // Not a routing model (or unknown): ordinary resolution handles it.
            _ => return Ok(None),
        }
    };

    // A hop advertises a protocol only when *every* target serves it
    // (Config::model_serves_protocol). Enforce the same rule at request time so a
    // protocol the client can see is exactly the protocol that resolves, and one
    // it cannot see is refused uniformly — never smuggled through by choosing a
    // level whose particular target happens to serve it.
    {
        let cfg = state.config();
        if !cfg.model_serves_protocol(&original_model_id, protocol) {
            let supported = Protocol::CLIENT_PROTOCOLS
                .iter()
                .filter(|p| cfg.model_serves_protocol(&original_model_id, **p))
                .map(|p| p.route_key())
                .collect::<Vec<_>>()
                .join(", ");
            let msg = format!(
                "model {original_model_id:?} does not support {} requests; supported protocols: {supported}",
                protocol.route_key()
            );
            tracing::warn!(
                model = %original_model_id,
                protocol = %protocol.route_key(),
                supported = %supported,
                "local request rejected: model does not support protocol"
            );
            return Err(Box::new(err(StatusCode::BAD_REQUEST, &msg)));
        }
    }

    let level = requested_reasoning_level(protocol, body).map(ToOwned::to_owned);
    let target = match level.as_deref() {
        Some(level) => hop.resolve_route(level).map(String::from),
        // No level supplied: the first entry is the declared default. Ordering
        // `reasoning_routes` is how that default is chosen.
        None => hop.routes().first().map(|entry| entry.target.clone()),
    };
    let Some(target) = target else {
        let available = hop
            .routes()
            .iter()
            .map(|e| e.level.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let msg = format!(
            "routing model {original_model_id:?} does not support reasoning level {:?}; available routes: {available}",
            level.as_deref().unwrap_or("(none)")
        );
        tracing::warn!(
            model = %original_model_id,
            requested_level = ?level,
            available_routes = %available,
            "local request rejected: routing model does not support requested reasoning level"
        );
        return Err(Box::new(err(StatusCode::BAD_REQUEST, &msg)));
    };

    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), json!(target));
    }
    Ok(Some(original_model_id))
}

/// §5.1.2: request-path errors split into client faults (400) and
/// proxy/configuration faults (500). The two classes must never be confused:
/// a misconfigured model or an inexpressible binding is not the client's
/// fault, must not be counted as a client bad request, and must not trigger
/// upstream cooldown or fallback.
#[derive(Debug)]
pub(super) enum ThinkingError {
    Client(String),
    ConfigAdapter(String),
}

impl std::fmt::Display for ThinkingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThinkingError::Client(msg) => write!(f, "{msg}"),
            ThinkingError::ConfigAdapter(msg) => write!(f, "{msg}"),
        }
    }
}

pub(super) fn resolve_request_candidates(
    state: &AppState,
    protocol: Protocol,
    body: &Value,
    client_model_id: &str,
    err: fn(StatusCode, &str) -> Response,
) -> Result<Vec<(String, ExecutionPlan)>, Box<Response>> {
    let Some(model_id) = body.get("model").and_then(Value::as_str) else {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            "missing required \"model\" field",
        )));
    };
    let cfg = state.config();
    let Some(model) = cfg.models.get(model_id) else {
        // the client-visible model ID is what the client asked for;
        // exposing the resolved target would leak internal topology.
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &format!(
                "unknown model {client_model_id:?}; llm-proxy does not substitute a default model — fix the client model ID, declare the model in config, or run `llm-proxy launch <client>` to refresh the client model list"
            ),
        )));
    };
    if !model.exposes_protocol(protocol) {
        let supported = model
            .supported_protocols()
            .iter()
            .map(|p| p.route_key())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &format!(
                "model {client_model_id:?} does not support {} requests; supported protocols: {supported}",
                protocol.route_key()
            ),
        )));
    }
    if let Some(missing) = missing_capability(model, protocol, body) {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &format!(
                "model {client_model_id:?} does not declare the {missing} capability required by this request"
            ),
        )));
    }
    if let Err(conflict) = reject_conflicting_anthropic_disabled(protocol, body) {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &conflict.to_string(),
        )));
    }
    if let Err(non_protocol) = reject_anthropic_non_protocol_fields(protocol, body) {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &non_protocol.to_string(),
        )));
    }
    // §5.1.1 rules 1-2 / §5.1.2: explicit-intent contradictions and
    // explicit-vs-force conflicts are rejected locally, before fingerprinting,
    // cooldown accounting, fallback selection, or any upstream contact.
    let intent = extract_thinking_intent(protocol, body);
    if intent.explicit_on() && intent.explicit_off() {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &format!(
                "conflicting explicit thinking signals: {} vs {}; remove one side or make the switch and level consistent",
                intent.on_fields.join(", "),
                intent.off_fields.join(", ")
            ),
        )));
    }
    if model.force_think == Some(true) && intent.explicit_off() {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &format!(
                "model {model_id:?} enforces force_think=true, but the request disables thinking via {}; remove the disabling field or adjust the model thinking strategy",
                intent.off_fields.join(", ")
            ),
        )));
    }
    if model.force_no_think == Some(true) && intent.explicit_on() {
        let fields = intent.on_fields.join(", ");
        let msg = format!(
            "model {model_id:?} enforces force_no_think=true, but the request enables thinking via {fields}; remove the enabling field or adjust the model thinking strategy"
        );
        tracing::warn!(
            model = %model_id,
            enabling_fields = %fields,
            "local request rejected: model enforces force_no_think"
        );
        return Err(Box::new(err(StatusCode::BAD_REQUEST, &msg)));
    }
    // Check requested level against the effective reasoning_levels table (ADR-034).
    // Close intent (none/disabled) does NOT go through table gating.
    // `minimal` is a Chat/Responses-only level with no Anthropic counterpart; when the
    // table has no `minimal` row it is gated as `low` (its default mapping), so the
    // table's `low` row is what must exist.
    if let Some(level) = requested_reasoning_level(protocol, body)
        && level != "none"
        && let Some(table) = cfg.effective_reasoning_levels(model_id)
    {
        let gate_level = gate_level_for(table, level);
        if !table.iter().any(|entry| entry.level() == gate_level) {
            let allowed = table
                .iter()
                .map(|entry| entry.level())
                .collect::<Vec<_>>()
                .join(", ");
            let msg = format!(
                "model {model_id:?} does not support reasoning level {level:?}; allowed levels: {allowed}"
            );
            tracing::warn!(
                model = %model_id,
                requested_level = %level,
                allowed_levels = %allowed,
                "local request rejected: model does not support reasoning level"
            );
            return Err(Box::new(err(StatusCode::BAD_REQUEST, &msg)));
        }
    }
    Ok(cfg.resolve_model_request_candidates(protocol, model_id))
}

/// The table level a client-requested level is checked (and mapped) against.
/// `minimal` has no Anthropic counterpart and defaults to `low`; every other
/// level maps to itself. Kept in lockstep with `mapped_reasoning_api_value`.
fn gate_level_for<'a>(table: &'a [crate::config::ReasoningLevelEntry], level: &'a str) -> &'a str {
    if level == "minimal" && !table.iter().any(|entry| entry.level() == level) {
        "low"
    } else {
        level
    }
}

pub(super) fn missing_capability(
    model: &ModelConfig,
    protocol: Protocol,
    body: &Value,
) -> Option<&'static str> {
    let declares = |feature: &str| model.features.iter().any(|f| f == feature);
    if request_has_image(protocol, body) && !declares("image_input") {
        return Some("image_input");
    }
    if request_has_audio(protocol, body) && !declares("audio_input") {
        return Some("audio_input");
    }
    if request_has_video(protocol, body) && !declares("video_input") {
        return Some("video_input");
    }
    if request_has_document(protocol, body) && !declares("document_input") {
        return Some("document_input");
    }
    None
}

pub(super) fn requested_reasoning_level(protocol: Protocol, body: &Value) -> Option<&str> {
    match protocol {
        // Top-level `reasoning_effort` first (legacy / pi-style); fall back to
        // nested `reasoning.effort` (qwen 0.24.7 default wire format).
        Protocol::OpenaiChatCompletions => body
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .or_else(|| {
                body.get("reasoning")
                    .and_then(|r| r.get("effort"))
                    .and_then(Value::as_str)
            }),
        Protocol::OpenaiResponses => body
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str),
        // Anthropic 4.6+: effort is at top-level output_config.effort, NOT thinking.effort.
        Protocol::Anthropic => body
            .get("output_config")
            .and_then(|oc| oc.get("effort"))
            .and_then(Value::as_str),
    }
}

/// §5.2.5: thinking intent captured from the *original* client body,
/// before any default injection. Later stages must never re-derive "client
/// explicit" from a mutated body. Each candidate rebuilds from the original
/// request plus this same entry intent.
#[derive(Debug, Default)]
pub(super) struct ThinkingIntent {
    on_fields: Vec<&'static str>,
    off_fields: Vec<&'static str>,
    has_level: bool,
}

impl ThinkingIntent {
    pub(super) fn explicit_on(&self) -> bool {
        !self.on_fields.is_empty()
    }
    pub(super) fn explicit_off(&self) -> bool {
        !self.off_fields.is_empty()
    }
}

pub(super) fn extract_thinking_intent(protocol: Protocol, body: &Value) -> ThinkingIntent {
    let mut intent = ThinkingIntent::default();
    // A level is always an intensity request (`has_level`). Whether it also counts as a
    // *switch* signal is protocol-dependent (ADR-034 §3a):
    // - Chat / Responses have no separate switch field, so `reasoning_effort` is their
    //   only thinking control and implies on (`!= none`) or off (`none`).
    // - Anthropic's `output_config.effort` is an independent knob ("thinking 开不开都能
    //   用"), so it never implies the switch. `disabled + effort` and `force_no_think`
    //   with only an effort are therefore not contradictions.
    // Unknown or empty values are not interpreted as intent (§5.1.1 rule 2: never guess
    // an unknown value into a budget or an on-expression).
    let take_level = |label: &'static str,
                      level: Option<&str>,
                      classify_switch: bool,
                      intent: &mut ThinkingIntent| {
        let Some(level) = level.filter(|level| !level.is_empty()) else {
            return;
        };
        intent.has_level = true;
        if !classify_switch {
            return;
        }
        if level == "none" {
            intent.off_fields.push(label);
        } else {
            intent.on_fields.push(label);
        }
    };
    match protocol {
        Protocol::OpenaiChatCompletions => {
            take_level(
                "reasoning_effort",
                requested_reasoning_level(protocol, body),
                true,
                &mut intent,
            );
        }
        Protocol::OpenaiResponses => {
            take_level(
                "reasoning.effort",
                requested_reasoning_level(protocol, body),
                true,
                &mut intent,
            );
        }
        Protocol::Anthropic => {
            // Anthropic 4.6+: effort lives at output_config.effort and is intensity-only.
            take_level(
                "output_config.effort",
                requested_reasoning_level(protocol, body),
                false,
                &mut intent,
            );
            if let Some(thinking) = body.get("thinking") {
                match thinking.get("type").and_then(Value::as_str) {
                    Some("adaptive") => intent.on_fields.push("thinking.type=adaptive"),
                    Some("disabled") => intent.off_fields.push("thinking.type=disabled"),
                    _ => {}
                }
            }
        }
    }
    intent
}

/// Rebuild one candidate's thinking body from the immutable original request
/// and the same entry intent. Defaults, level maps, and force-on expressions
/// are applied to a clone; they never mutate the original body.
pub(super) fn prepare_candidate_thinking_body(
    state: &AppState,
    protocol: Protocol,
    original_body: &Value,
    model_id: &str,
    plan: &ExecutionPlan,
    intent: &ThinkingIntent,
) -> Result<(Value, bool), ThinkingError> {
    let thinking_disabled = thinking_is_disabled(
        state,
        protocol,
        original_body,
        model_id,
        &plan.provider_id,
        intent,
    );
    let body = body_with_default_reasoning(
        state,
        protocol,
        plan.source_protocol,
        original_body.clone(),
        intent,
    );
    let body = body_with_mapped_reasoning(state, protocol, body, model_id, plan, intent)?;
    Ok((body, thinking_disabled))
}

/// `upstream_protocol` is the protocol the request will actually be sent in (the
/// candidate's `source_protocol`). A few Layer B rules cannot be decided on the ingress
/// shape alone — notably the Anthropic-only "on, no level" state, which the peer
/// protocols cannot express (spec.md §5.2).
pub(super) fn body_with_default_reasoning(
    state: &AppState,
    protocol: Protocol,
    upstream_protocol: Protocol,
    mut body: Value,
    intent: &ThinkingIntent,
) -> Value {
    let Some(model_id) = body.get("model").and_then(Value::as_str) else {
        return body;
    };
    let cfg = state.config();
    let Some(model) = cfg.models.get(model_id) else {
        return body;
    };
    // §5.1.1 rule 3: explicit off is authoritative; nothing is filled. The
    // per-candidate stage strips reasoning fields and applies the endpoint's
    // off-dialect (a force-on conflict was already rejected at ingress).
    if intent.explicit_off() {
        return body;
    }
    // spec.md §5.2: Anthropic `adaptive` with no level is "explicit on, no level". The
    // client's explicit on beats `default_think=false` on Anthropic egress; the peer
    // protocols cannot express the shape, so there it degrades to the closed state.
    if !intent.has_level
        && intent.explicit_on()
        && model.default_think == Some(false)
        && protocol == Protocol::Anthropic
    {
        if upstream_protocol == Protocol::Anthropic {
            return body;
        }
        return body_without_reasoning(protocol, body);
    }
    // Strategy-driven effective off (rules 3, 6): force-off always strips;
    // default-off strips only when no explicit on-signal overrides it.
    if model.force_no_think == Some(true)
        || (model.default_think == Some(false) && !intent.explicit_on())
    {
        // force_no_think + default level → disabled + output_config.effort (ADR-034).
        if model.force_no_think == Some(true)
            && let Some(default_level) = model.default_reasoning_level.as_deref()
            && protocol == Protocol::Anthropic
        {
            let obj = body.as_object_mut().unwrap();
            obj.insert("thinking".to_string(), json!({"type": "disabled"}));
            obj.entry("output_config")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .map(|oc| oc.insert("effort".to_string(), json!(default_level)));
            return body;
        }
        return body_without_reasoning(protocol, body);
    }
    // Rule 3/4/5: a default level only fills what is still missing; explicit
    // levels are never replaced or topped up. force-on without a level uses the
    // candidate's defined on-expression, not a guessed default.
    if intent.has_level || model.force_think == Some(true) {
        return body;
    }
    let Some(default_level) = model.default_reasoning_level.as_deref() else {
        return body;
    };
    if protocol == Protocol::Anthropic
        && let Some(thinking) = body.get("thinking")
    {
        // Only top up a clearly adaptive thinking config; never rewrite opaque
        // or unknown thinking objects (disabled already returned above).
        if thinking.get("type").and_then(Value::as_str) != Some("adaptive") {
            return body;
        }
    }
    // A configured default level is an explicit on-intent: materialize the Anthropic
    // switch so the body becomes `adaptive + effort` (spec.md §5.2).
    set_reasoning_level(protocol, &mut body, default_level, true);
    body
}

pub(super) fn thinking_is_disabled(
    state: &AppState,
    protocol: Protocol,
    body: &Value,
    model_id: &str,
    provider_id: &str,
    intent: &ThinkingIntent,
) -> bool {
    if intent.explicit_off() || requested_reasoning_level(protocol, body) == Some("none") {
        return true;
    }
    if intent.explicit_on() {
        // force-off vs explicit-on is rejected at ingress; mapped still honors force-off.
        return state
            .config()
            .models
            .get(model_id)
            .is_some_and(|model| model.force_no_think == Some(true));
    }
    effective_enable_thinking(state, model_id, provider_id) == Some(false)
}

pub(super) fn body_with_mapped_reasoning(
    state: &AppState,
    protocol: Protocol,
    mut body: Value,
    model_id: &str,
    plan: &ExecutionPlan,
    intent: &ThinkingIntent,
) -> Result<Value, ThinkingError> {
    reject_conflicting_anthropic_disabled(protocol, &body)?;
    let requested_level = requested_reasoning_level(protocol, &body).map(ToOwned::to_owned);
    // Anthropic disabled passthrough: if thinking.type=disabled, pass through as-is.
    if intent.explicit_off()
        && protocol == Protocol::Anthropic
        && body
            .get("thinking")
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            == Some("disabled")
    {
        return Ok(body);
    }
    if thinking_is_disabled(state, protocol, &body, model_id, &plan.provider_id, intent) {
        return Ok(body_without_reasoning(protocol, body));
    }

    // §5.2.2 rule 5 / ADR-034 §3: force-on materialization.
    // When `default_reasoning_level` is set, the on-expression is that configured
    // level. When absent (e.g. Claude force_think without level control), the
    // converter injects the upstream thinking config — nothing to do here.
    let config = state.config();
    if let Some(model) = config.models.get(model_id)
        && model.force_think == Some(true)
        && !intent.has_level
        && !intent.explicit_on()
    {
        if let Some(default_level) = model.default_reasoning_level.as_deref() {
            set_reasoning_level(protocol, &mut body, default_level, true);
        }
        // Dialects whose only on-expression is a toggle need it alongside the level.
        if matches!(
            plan.compat.thinking_format.as_deref(),
            Some("qwen" | "qwen-enable-thinking")
        ) && let Some(obj) = body.as_object_mut()
        {
            obj.insert("enable_thinking".to_string(), json!(true));
        }
    }

    let Some(level) = requested_level else {
        return Ok(body);
    };
    // Close intent ("none") does not go through the table.
    if level == "none" {
        return Ok(body_without_reasoning(protocol, body));
    }
    let Some(api_value) = mapped_reasoning_api_value(state, model_id, &level)
        .map_err(|e| ThinkingError::ConfigAdapter(e.to_string()))?
    else {
        return Err(ThinkingError::ConfigAdapter(format!(
            "model {model_id:?} reasoning level {level:?} is not expressible for provider {:?}; adjust the model's reasoning_levels table or remove the level",
            plan.provider_id
        )));
    };
    // Client-supplied level: rewrite only the level, never materialize the Anthropic
    // switch — `output_config.effort` alone means "do not touch the switch" (spec.md §5.2).
    set_reasoning_level(protocol, &mut body, &api_value, false);
    Ok(body)
}

fn reject_conflicting_anthropic_disabled(
    protocol: Protocol,
    body: &Value,
) -> Result<(), ThinkingError> {
    if protocol != Protocol::Anthropic {
        return Ok(());
    }
    let Some(thinking) = body.get("thinking").and_then(Value::as_object) else {
        return Ok(());
    };
    if thinking.get("type").and_then(Value::as_str) != Some("disabled") {
        return Ok(());
    }
    // disabled + output_config.effort is LEGAL (ADR-034 §3a).
    // Still reject disabled + non-protocol fields inside thinking object.
    if let Some(field) = ["effort", "level", "budget_tokens"]
        .into_iter()
        .find(|field| thinking.contains_key(*field))
    {
        return Err(ThinkingError::Client(format!(
            "Anthropic thinking.type \"disabled\" conflicts with thinking.{field}; remove the non-protocol field from the thinking object"
        )));
    }
    Ok(())
}

/// Reject non-protocol fields for Anthropic 4.6+ (§4.1).
/// - `thinking.effort` / `thinking.level`: moved to `output_config.effort`
/// - `thinking.type=enabled` or any unknown/absent `type`: only `adaptive` / `disabled`
///   are accepted; never guessed into one of them
/// - `thinking.budget_tokens`: not a protocol field for 4.6+
/// - `enable_thinking`: not a protocol field for any of the 3 ingress protocols
fn reject_anthropic_non_protocol_fields(
    protocol: Protocol,
    body: &Value,
) -> Result<(), ThinkingError> {
    // enable_thinking is rejected for ALL ingress protocols (ADR-034 §4).
    if body.get("enable_thinking").is_some() {
        return Err(ThinkingError::Client(
            "enable_thinking is not a protocol field; remove it from the request".to_string(),
        ));
    }
    if protocol != Protocol::Anthropic {
        return Ok(());
    }
    let Some(thinking_value) = body.get("thinking") else {
        return Ok(());
    };
    let Some(thinking) = thinking_value.as_object() else {
        return Err(ThinkingError::Client(
            "thinking must be an object with type \"adaptive\" or \"disabled\"".to_string(),
        ));
    };
    // thinking.effort / thinking.level are non-protocol fields in 4.6+.
    if let Some(field) = ["effort", "level"]
        .into_iter()
        .find(|field| thinking.contains_key(*field))
    {
        return Err(ThinkingError::Client(format!(
            "thinking.{field} is not a protocol field; use output_config.effort for effort level"
        )));
    }
    // thinking.budget_tokens is not a protocol field for 4.6+.
    if thinking.contains_key("budget_tokens") {
        return Err(ThinkingError::Client(
            "thinking.budget_tokens is not a protocol field; use output_config.effort for effort level"
                .to_string(),
        ));
    }
    // Only `adaptive` / `disabled` are accepted. `enabled` is deprecated; unknown or
    // absent types are never guessed into one of the two valid values.
    match thinking.get("type").and_then(Value::as_str) {
        Some("adaptive") | Some("disabled") => {}
        Some("enabled") => {
            return Err(ThinkingError::Client(
                "thinking.type \"enabled\" is deprecated; use \"adaptive\" with output_config.effort"
                    .to_string(),
            ));
        }
        other => {
            return Err(ThinkingError::Client(match other {
                Some(other) => {
                    format!(
                        "unknown thinking.type {other:?}; expected \"adaptive\" or \"disabled\""
                    )
                }
                None => {
                    "thinking.type is required; expected \"adaptive\" or \"disabled\"".to_string()
                }
            }));
        }
    }
    // `output_config.effort=none` is not an Anthropic enum value: Anthropic closes via
    // `thinking.type=disabled`. An empty value is a format error and is never guessed.
    match body
        .get("output_config")
        .and_then(|config| config.get("effort"))
    {
        Some(Value::String(effort)) if effort == "none" => Err(ThinkingError::Client(
            "output_config.effort \"none\" is not an Anthropic value; disable thinking with thinking.type \"disabled\" and omit output_config.effort"
                .to_string(),
        )),
        Some(Value::String(effort)) if effort.is_empty() => Err(ThinkingError::Client(
            "output_config.effort must not be empty; omit it to leave the effort unset".to_string(),
        )),
        _ => Ok(()),
    }
}

pub(super) fn effective_enable_thinking(
    state: &AppState,
    model_id: &str,
    _provider_id: &str,
) -> Option<bool> {
    // Strategy: force-on/off override default; Provider no longer inherits (§5.1).
    let cfg = state.config();
    let model = cfg.models.get(model_id)?;
    if model.force_think == Some(true) {
        return Some(true);
    }
    if model.force_no_think == Some(true) {
        return Some(false);
    }
    model.default_think
}

pub(super) fn mapped_reasoning_api_value(
    state: &AppState,
    model_id: &str,
    level: &str,
) -> Result<Option<String>> {
    let cfg = state.config();
    // Whole-table override (ADR-034 §2): the Model's own table replaces the bound
    // provider's table entirely; it only falls back when the Model declares none.
    let effective_table = cfg.effective_reasoning_levels(model_id);

    if let Some(table) = effective_table {
        if let Some(entry) = table.iter().find(|entry| entry.level() == level) {
            // Found in table: use api_value (None = identity).
            return Ok(Some(
                entry
                    .api_value()
                    .map(str::to_owned)
                    .unwrap_or_else(|| entry.level().to_string()),
            ));
        }
        // `minimal` default mapping: if not in table, map to `low` (Chat/Responses → Anthropic).
        if level == "minimal"
            && let Some(low_entry) = table.iter().find(|entry| entry.level() == "low")
        {
            return Ok(Some(
                low_entry
                    .api_value()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "low".to_string()),
            ));
        }
        // Level not in table → not expressible.
        return Ok(None);
    }
    // No table → identity passthrough.
    Ok(Some(level.to_string()))
}

/// Write a level into the ingress-protocol body.
///
/// On Anthropic the level lives in `output_config.effort` while the switch lives in
/// `thinking.type` — two independent knobs (spec.md §5.2). `materialize_switch` decides
/// whether a missing `thinking` object is created:
/// - `true` for config-driven fills (a configured default level or force-on is an
///   explicit on-intent, so the body becomes `adaptive + effort`);
/// - `false` for client-supplied levels, because `output_config.effort` alone means
///   "set the intensity, do not touch the switch" (spec.md §5.2) — inventing `adaptive`
///   there would silently enable thinking.
pub(super) fn set_reasoning_level(
    protocol: Protocol,
    body: &mut Value,
    api_value: &str,
    materialize_switch: bool,
) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    match protocol {
        Protocol::OpenaiChatCompletions => {
            obj.insert("reasoning_effort".to_string(), json!(api_value));
        }
        Protocol::OpenaiResponses => {
            if let Some(reasoning) = obj
                .entry("reasoning")
                .or_insert_with(|| json!({}))
                .as_object_mut()
            {
                reasoning.insert("effort".to_string(), json!(api_value));
            }
        }
        Protocol::Anthropic => {
            if materialize_switch {
                if let Some(thinking) = obj
                    .entry("thinking")
                    .or_insert_with(|| json!({"type": "adaptive"}))
                    .as_object_mut()
                {
                    thinking.insert("type".to_string(), json!("adaptive"));
                }
            } else if let Some(thinking) = obj.get_mut("thinking").and_then(Value::as_object_mut) {
                thinking.insert("type".to_string(), json!("adaptive"));
            }
            if let Some(output_config) = obj
                .entry("output_config")
                .or_insert_with(|| json!({}))
                .as_object_mut()
            {
                output_config.insert("effort".to_string(), json!(api_value));
            }
        }
    }
}

// the parameter is the *execution* identity (the resolved target),
// not the client-visible model. The caller must pass the target so the lookup
// finds the target's service-tier declaration.
pub(super) fn apply_codex_service_tier(state: &AppState, exec_model: &str, body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let tier_config = state
        .config()
        .models
        .get(exec_model)
        .and_then(|model| model.codex_service_tier.clone());
    let Some(tier_config) = tier_config else {
        return;
    };
    if let Some(forced_tier) = tier_config.forced_tier {
        obj.insert("service_tier".to_string(), json!(forced_tier));
    } else if !obj.contains_key("service_tier")
        && let Some(default_tier) = tier_config.default_tier
    {
        obj.insert("service_tier".to_string(), json!(default_tier));
    }
}

/// Express an effective thinking-off state on the ingress protocol.
///
/// Only the *switch* is closed; sibling intensity data is preserved:
/// - Anthropic: `thinking.type=disabled`; `output_config` is left alone so a client's
///   `output_config.effort` survives as the legal "off + intensity" shape (spec.md §5.2),
///   including its non-`effort` keys (spec.md §5.2).
/// - Chat / Responses: the protocol-standard off value (`none`), never a bare removal —
///   removing the field would read as "unset" and leave the upstream free to think, and
///   it would erase the off signal before a cross-protocol conversion (spec.md §5.2).
///   Sibling keys such as `reasoning.summary` survive (spec.md §5.2).
pub(super) fn body_without_reasoning(protocol: Protocol, mut body: Value) -> Value {
    let Some(obj) = body.as_object_mut() else {
        return body;
    };
    match protocol {
        Protocol::OpenaiChatCompletions => {
            obj.insert("reasoning_effort".to_string(), json!("none"));
        }
        Protocol::OpenaiResponses => {
            if let Some(reasoning) = obj
                .entry("reasoning")
                .or_insert_with(|| json!({}))
                .as_object_mut()
            {
                reasoning.insert("effort".to_string(), json!("none"));
            }
        }
        Protocol::Anthropic => {
            obj.insert("thinking".to_string(), json!({"type": "disabled"}));
        }
    }
    body
}

pub(super) fn apply_disabled_thinking_egress(
    mut body: Value,
    plan: &ExecutionPlan,
    thinking_disabled: bool,
) -> Value {
    if !thinking_disabled {
        return body;
    }
    let Some(obj) = body.as_object_mut() else {
        return body;
    };

    // Off-expression dialect comes from the *target native endpoint's* compat
    // (ADR-034 §5). `chat_reasoning_off_format` is a retired 053-era key that no
    // longer drives cross-protocol off-expression selection.
    match plan.compat.thinking_format.as_deref() {
        Some("deepseek" | "mimo-thinking-toggle" | "anthropic-thinking") => {
            obj.remove("reasoning_effort");
            obj.remove("reasoning");
            obj.insert("thinking".to_string(), json!({"type": "disabled"}));
        }
        Some("openai-responses-reasoning") => {
            obj.remove("reasoning_effort");
            obj.remove("thinking");
            obj.insert("reasoning".to_string(), json!({"effort": "none"}));
        }
        Some("qwen" | "qwen-enable-thinking") => {
            obj.remove("reasoning_effort");
            obj.remove("reasoning");
            obj.insert("enable_thinking".to_string(), json!(false));
        }
        _ => {}
    }
    body
}

/// Strip vendor-issued signature fields from the egress body.
///
/// Two ingress shapes carry signatures:
/// - **Anthropic**: `thinking[]` blocks may have a `signature` field.
/// - **Responses**: `input[]` reasoning items may have a `signature` field.
///
/// A signature is only valid for replay to the endpoint that issued it, so it
/// must not be forwarded to a different upstream.
pub(super) fn strip_vendor_signatures(mut body: Value) -> Value {
    let Some(obj) = body.as_object_mut() else {
        return body;
    };
    // Anthropic ingress: thinking blocks
    if let Some(thinking) = obj.get_mut("thinking").and_then(Value::as_array_mut) {
        for block in thinking {
            if let Some(block_obj) = block.as_object_mut() {
                block_obj.remove("signature");
            }
        }
    }
    // Responses ingress: reasoning items in input array
    if let Some(input) = obj.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            if let Some(item_obj) = item.as_object_mut()
                && item_obj.get("type").and_then(Value::as_str) == Some("reasoning")
            {
                item_obj.remove("signature");
            }
        }
    }
    body
}

pub(super) fn request_has_image(protocol: Protocol, body: &Value) -> bool {
    content_part_types(protocol, body)
        .any(|kind| matches!(kind, "image_url" | "input_image" | "image"))
}

pub(super) fn request_has_audio(protocol: Protocol, body: &Value) -> bool {
    content_part_types(protocol, body).any(|kind| kind == "input_audio")
}

pub(super) fn request_has_video(protocol: Protocol, body: &Value) -> bool {
    content_part_types(protocol, body).any(|kind| kind == "video_url")
}

pub(super) fn request_has_document(protocol: Protocol, body: &Value) -> bool {
    content_part_types(protocol, body)
        .any(|kind| matches!(kind, "file" | "input_file" | "document"))
}

pub(super) fn content_part_types<'a>(
    protocol: Protocol,
    body: &'a Value,
) -> Box<dyn Iterator<Item = &'a str> + 'a> {
    match protocol {
        Protocol::OpenaiChatCompletions | Protocol::Anthropic => {
            let messages = body
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten();
            Box::new(messages.flat_map(|message| {
                message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.get("type").and_then(Value::as_str))
            }))
        }
        Protocol::OpenaiResponses => {
            let items = body
                .get("input")
                .and_then(Value::as_array)
                .into_iter()
                .flatten();
            Box::new(items.flat_map(|item| {
                item.get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.get("type").and_then(Value::as_str))
            }))
        }
    }
}
