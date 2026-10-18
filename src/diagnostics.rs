//! Structured diagnostics for the 4xx failure classes that used to
//! leave zero server-side trace — upstream `INVALID_ARGUMENT` rejections and
//! local egress-preflight rejections.
//!
//! Capture contract (user ruling 2026-09-20): *every* feature of a failing
//! request is recorded except literal content; content is recorded as metadata
//! (counts, lengths, digests). `capture = "full"` additionally embeds the
//! upstream egress body — safe because egress envelopes carry no
//! credentials (auth lives in headers) — but only after the envelope key set
//! is verified, and always bounded by `max_record_bytes`; the JSONL file
//! itself is capped by `file_max_bytes` (newest half kept on overflow).

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::config::DiagnosticsConfig;

#[allow(dead_code)]
pub(crate) const EVENT_EGRESS_PREFLIGHT: &str = "egress_preflight";
#[allow(dead_code)]
pub(crate) const EVENT_UPSTREAM_CLIENT_ERROR: &str = "upstream_client_error";
#[allow(dead_code)]
pub(crate) const EVENT_LOCAL_AUTH_FAILURE: &str = "local_auth_failure";

/// Diagnostics capture mode, governing whether `body_full` may appear in records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureMode {
    Metadata,
    Full,
}

/// The closed key set the upstream egress envelope emits.
/// `full` capture refuses any body with unknown top-level keys, so a future
/// envelope change can never smuggle a credential into the diagnostics file.
const ENVELOPE_KEYS: [&str; 6] = [
    "model",
    "project",
    "userAgent",
    "requestType",
    "requestId",
    "request",
];

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FieldViolation {
    pub field: String,
    pub description: String,
}

/// Metadata-only request features: shapes and sizes, never literal content.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct RequestFeatures {
    pub body_bytes: u64,
    pub body_sha256: String,
    pub top_level_keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_count: Option<usize>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub role_counts: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub part_type_counts: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_len_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_len_max: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_text_len: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_decl_count: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_names: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub function_calls: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub schema_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<Value>,
}

impl RequestFeatures {
    /// Digest-only view for bodies in a format this module does not model.
    #[allow(dead_code)]
    pub(crate) fn from_opaque_body(body: &Value) -> Self {
        let text = body.to_string();
        Self {
            body_bytes: text.len() as u64,
            body_sha256: digest(&text),
            top_level_keys: body
                .as_object()
                .map(|obj| obj.keys().cloned().collect())
                .unwrap_or_default(),
            content_count: None,
            role_counts: BTreeMap::new(),
            part_type_counts: BTreeMap::new(),
            text_len_total: None,
            text_len_max: None,
            system_text_len: None,
            tool_decl_count: None,
            tool_names: Vec::new(),
            function_calls: Vec::new(),
            schema_types: Vec::new(),
            generation_config: None,
        }
    }
}

#[allow(dead_code)]
fn collect_schema_types(schema: &Value, out: &mut Vec<String>) {
    let Some(obj) = schema.as_object() else {
        return;
    };
    if let Some(kind) = obj.get("type").and_then(Value::as_str) {
        out.push(kind.to_string());
    }
    if let Some(properties) = obj.get("properties").and_then(Value::as_object) {
        for child in properties.values() {
            collect_schema_types(child, out);
        }
    }
    if let Some(items) = obj.get("items") {
        collect_schema_types(items, out);
    }
}

#[allow(dead_code)]
fn digest(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let full = hasher.finalize();
    full.iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DiagnosticRecord {
    pub ts: String,
    pub event: &'static str,
    pub provider: String,
    pub upstream_model: String,
    pub frontend_model: String,
    pub protocol: String,
    pub adapter: String,
    pub stream: bool,
    /// Frontend token identifier (not a secret): the subject of the
    /// (token × provider) cooldown for upstream 400s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_message: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub field_violations: Vec<FieldViolation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub upstream_detail_types: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<Value>,
    pub features: RequestFeatures,
    /// Egress body candidate; only embedded when `capture = "full"` and the
    /// envelope key set is exactly the expected agent shape.
    #[serde(skip)]
    pub body_candidate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_full: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl DiagnosticRecord {
    pub(crate) fn new(event: &'static str, features: RequestFeatures) -> Self {
        Self {
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            event,
            provider: String::new(),
            upstream_model: String::new(),
            frontend_model: String::new(),
            protocol: String::new(),
            adapter: String::new(),
            stream: false,
            token: None,
            upstream_status: None,
            upstream_message: None,
            field_violations: Vec::new(),
            upstream_detail_types: Vec::new(),
            violations: Vec::new(),
            features,
            body_candidate: None,
            body_full: None,
            truncated: false,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn with_violations(mut self, violations: &[Value]) -> Self {
        self.violations = violations.to_vec();
        self
    }

    #[allow(dead_code)]
    pub(crate) fn with_details(
        mut self,
        field_violations: Vec<FieldViolation>,
        upstream_detail_types: Vec<String>,
    ) -> Self {
        self.field_violations = field_violations;
        self.upstream_detail_types = upstream_detail_types;
        self
    }
}

pub(crate) struct DiagnosticsStore {
    config: DiagnosticsConfig,
    jsonl_path: PathBuf,
    io_lock: Mutex<()>,
}

impl DiagnosticsStore {
    pub(crate) fn with_paths(config: DiagnosticsConfig, jsonl_path: PathBuf) -> Result<Self> {
        if let Some(parent) = jsonl_path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Self {
            config,
            jsonl_path,
            io_lock: Mutex::new(()),
        })
    }

    #[cfg(test)]
    pub(crate) fn jsonl_path(&self) -> &std::path::Path {
        &self.jsonl_path
    }

    /// Current capture mode — governs whether `body_full` may appear in records.
    pub(crate) fn capture_mode(&self) -> CaptureMode {
        if self.config.capture == "full" {
            CaptureMode::Full
        } else {
            CaptureMode::Metadata
        }
    }

    /// Append one record; failures are logged and never propagate into the
    /// request path. Rotation keeps the file bounded at `file_max_bytes`.
    pub(crate) fn record(&self, mut record: DiagnosticRecord) {
        if !self.config.enabled {
            return;
        }
        tracing::warn!(
            event = record.event,
            provider = %record.provider,
            model = %record.upstream_model,
            status = ?record.upstream_status,
            violations = record.violations.len(),
            "request failed preflight or was rejected upstream"
        );
        record.body_full = self.capture_body(record.body_candidate.take());
        let serialized = match serde_json::to_string(&record) {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!("diagnostics serialization failed: {error}");
                return;
            }
        };
        let _guard = match self.io_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let appended = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.jsonl_path)?;
            writeln!(file, "{serialized}")?;
            self.rotate_if_oversized()?;
            Ok(())
        })();
        if let Err(error) = appended {
            tracing::warn!("diagnostics write failed: {error}");
        }
    }

    fn capture_body(&self, candidate: Option<String>) -> Option<String> {
        if self.config.capture != "full" {
            return None;
        }
        let body = candidate?;
        let parsed: Value = match serde_json::from_str(&body) {
            Ok(parsed) => parsed,
            Err(_) => return None,
        };
        let is_known_envelope = parsed.as_object().is_some_and(|obj| {
            obj.len() == ENVELOPE_KEYS.len()
                && ENVELOPE_KEYS.iter().all(|key| obj.contains_key(*key))
        });
        if !is_known_envelope {
            tracing::warn!("diagnostics full capture refused: unknown envelope keys");
            return None;
        }
        if body.len() <= self.config.max_record_bytes {
            return Some(body);
        }
        // Truncate on a char boundary.
        let mut cut = self.config.max_record_bytes;
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        Some(body[..cut].to_string())
    }

    fn rotate_if_oversized(&self) -> Result<()> {
        let size = fs::metadata(&self.jsonl_path).map(|m| m.len()).unwrap_or(0);
        if size <= self.config.file_max_bytes {
            return Ok(());
        }
        let file = fs::File::open(&self.jsonl_path)?;
        let lines: Vec<String> = BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter(|line| !line.trim().is_empty())
            .collect();
        let keep = lines.len().div_ceil(2).max(1);
        let kept = &lines[lines.len() - keep.min(lines.len())..];
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.jsonl_path)?;
        for line in kept {
            writeln!(file, "{line}")?;
        }
        Ok(())
    }

    /// Test / operator aid: parse the JSONL file back into records.
    #[cfg(test)]
    pub(crate) fn read_records(&self) -> Vec<Value> {
        let Ok(file) = fs::File::open(&self.jsonl_path) else {
            return Vec::new();
        };
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect()
    }

    /// Production query: filter records by event type and time range, with
    /// pagination. `start_ts` / `end_ts` are UTC seconds since epoch; records
    /// whose `ts` falls outside `[start_ts, end_ts)` are excluded. Either bound
    /// may be `None` to leave that side open. Returns `(page_records, total_matched)`.
    pub(crate) fn query_records(
        &self,
        event: Option<&str>,
        start_ts: Option<i64>,
        end_ts: Option<i64>,
        limit: usize,
        offset: usize,
    ) -> (Vec<Value>, usize) {
        let Ok(file) = fs::File::open(&self.jsonl_path) else {
            return (Vec::new(), 0);
        };
        let all_records: Vec<Value> = BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect();

        let filtered: Vec<Value> = all_records
            .into_iter()
            .filter(|record| {
                if let Some(event) = event
                    && record.get("event").and_then(Value::as_str) != Some(event)
                {
                    return false;
                }
                if start_ts.is_some() || end_ts.is_some() {
                    let record_ts = record
                        .get("ts")
                        .and_then(Value::as_str)
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.timestamp())
                        .unwrap_or(0);
                    if let Some(start) = start_ts
                        && record_ts < start
                    {
                        return false;
                    }
                    if let Some(end) = end_ts
                        && record_ts >= end
                    {
                        return false;
                    }
                }
                true
            })
            .collect();

        let total = filtered.len();
        let page: Vec<Value> = filtered.into_iter().skip(offset).take(limit).collect();
        (page, total)
    }
}

/// Trim upstream error text to a bounded, single-line summary.
pub(crate) fn bounded_message(text: &str, max_bytes: usize) -> String {
    let mut cut = text.len().min(max_bytes);
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].replace(['\n', '\r'], " ")
}

/// Parse a Google-RPC error body into structured `fieldViolations`
/// (fixture shape: `details[].@type == google.rpc.BadRequest`).
#[allow(dead_code)]
pub(crate) fn extract_google_field_violations(error_json: &Value) -> Vec<FieldViolation> {
    let mut out = Vec::new();
    let Some(details) = error_json
        .get("error")
        .and_then(|e| e.get("details"))
        .and_then(Value::as_array)
    else {
        return out;
    };
    for detail in details {
        let Some(violations) = detail
            .get("fieldViolations")
            .or_else(|| detail.get("field_violations"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for violation in violations {
            out.push(FieldViolation {
                field: violation
                    .get("field")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                description: violation
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    out
}

/// Detail `@type` identifiers present on an upstream error body.
pub(crate) fn extract_upstream_detail_types(error_json: &Value) -> Vec<String> {
    error_json
        .get("error")
        .and_then(|e| e.get("details"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|detail| {
            detail
                .get("@type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn features_for(record: RequestFeatures) -> RequestFeatures {
        record
    }

    fn sample_record(event: &'static str) -> DiagnosticRecord {
        let mut record = DiagnosticRecord::new(
            event,
            features_for(RequestFeatures::from_opaque_body(
                &json!({"hello": "world"}),
            )),
        );
        record.provider = "p".into();
        record
    }

    fn temp_store() -> (DiagnosticsStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiagnosticsStore::with_paths(
            DiagnosticsConfig::default(),
            dir.path().join("diagnostics.jsonl"),
        )
        .expect("store");
        (store, dir)
    }

    #[test]
    fn metadata_mode_never_embeds_body() {
        let (store, _dir) = temp_store();
        let mut record = sample_record(EVENT_EGRESS_PREFLIGHT);
        record.body_candidate = Some("secret prompt body".to_string());
        store.record(record);
        let records = store.read_records();
        assert_eq!(records.len(), 1);
        assert!(records[0].get("body_full").is_none());
        let raw = std::fs::read_to_string(store.jsonl_path()).expect("file");
        assert!(!raw.contains("secret prompt body"));
    }

    #[test]
    fn full_mode_embeds_known_envelope_and_truncates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = DiagnosticsConfig {
            capture: "full".to_string(),
            max_record_bytes: 64,
            ..Default::default()
        };
        let store = DiagnosticsStore::with_paths(config, dir.path().join("diagnostics.jsonl"))
            .expect("store");
        let huge_body = json!({
            "model": "m", "project": "p", "userAgent": "some-agent",
            "requestType": "agent", "requestId": "r",
            "request": {"contents": [{"role": "user", "parts": [{"text": "x".repeat(500)}]}]}
        })
        .to_string();
        let mut record = sample_record(EVENT_UPSTREAM_CLIENT_ERROR);
        record.body_candidate = Some(huge_body.clone());
        store.record(record);
        let records = store.read_records();
        let body = records[0]["body_full"].as_str().expect("captured body");
        assert!(body.len() <= 64, "body must be capped: {}", body.len());
    }

    #[test]
    fn full_mode_refuses_unknown_envelope_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiagnosticsStore::with_paths(
            DiagnosticsConfig {
                capture: "full".to_string(),
                ..Default::default()
            },
            dir.path().join("diagnostics.jsonl"),
        )
        .expect("store");
        let mut record = sample_record(EVENT_UPSTREAM_CLIENT_ERROR);
        record.body_candidate = Some(json!({"model": "m", "bearerToken": "secret"}).to_string());
        store.record(record);
        let records = store.read_records();
        assert!(records[0].get("body_full").is_none());
        let raw = std::fs::read_to_string(store.jsonl_path()).expect("file");
        assert!(!raw.contains("bearerToken"));
    }

    #[test]
    fn rotation_keeps_newest_half() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiagnosticsStore::with_paths(
            DiagnosticsConfig {
                file_max_bytes: 65_536,
                ..Default::default()
            },
            dir.path().join("diagnostics.jsonl"),
        )
        .expect("store");
        for _ in 0..300 {
            store.record(sample_record(EVENT_EGRESS_PREFLIGHT));
        }
        let size = fs::metadata(store.jsonl_path()).expect("meta").len();
        assert!(size <= 65_536, "file must stay capped: {size}");
        let records = store.read_records();
        assert!(!records.is_empty());
    }

    #[test]
    fn disabled_store_records_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiagnosticsStore::with_paths(
            DiagnosticsConfig {
                enabled: false,
                ..Default::default()
            },
            dir.path().join("diagnostics.jsonl"),
        )
        .expect("store");
        store.record(sample_record(EVENT_EGRESS_PREFLIGHT));
        assert!(store.read_records().is_empty());
    }

    #[test]
    fn google_field_violations_extracted_from_shape() {
        let body = json!({
            "error": {
                "code": 400,
                "status": "INVALID_ARGUMENT",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.BadRequest",
                    "fieldViolations": [
                        {"field": "request.contents[0].parts[0].text", "description": "invalid"}
                    ]
                }]
            }
        });
        let violations = extract_google_field_violations(&body);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].field, "request.contents[0].parts[0].text");
        assert_eq!(
            extract_upstream_detail_types(&body),
            vec!["type.googleapis.com/google.rpc.BadRequest".to_string()]
        );
        assert!(extract_google_field_violations(&json!({"error": {"code": 400}})).is_empty());
    }

    /// : `query_records` filters by event type and supports pagination.
    #[test]
    fn query_records_filters_by_event_and_paginates() {
        let (store, _dir) = temp_store();
        for _ in 0..3 {
            store.record(sample_record(EVENT_EGRESS_PREFLIGHT));
        }
        for _ in 0..2 {
            store.record(sample_record(EVENT_UPSTREAM_CLIENT_ERROR));
        }

        // Filter by egress_preflight — must return exactly 3.
        let (records, total) =
            store.query_records(Some(EVENT_EGRESS_PREFLIGHT), None, None, 100, 0);
        assert_eq!(total, 3);
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|r| r["event"] == EVENT_EGRESS_PREFLIGHT));

        // Filter by upstream_client_error — must return exactly 2.
        let (records, total) =
            store.query_records(Some(EVENT_UPSTREAM_CLIENT_ERROR), None, None, 100, 0);
        assert_eq!(total, 2);
        assert_eq!(records.len(), 2);

        // No filter — returns all 5.
        let (records, total) = store.query_records(None, None, None, 100, 0);
        assert_eq!(total, 5);
        assert_eq!(records.len(), 5);

        // Pagination: limit=2, offset=0 returns first 2 of 5.
        let (records, total) = store.query_records(None, None, None, 2, 0);
        assert_eq!(total, 5);
        assert_eq!(records.len(), 2);

        // Pagination: limit=2, offset=4 returns last 1 of 5.
        let (records, total) = store.query_records(None, None, None, 2, 4);
        assert_eq!(total, 5);
        assert_eq!(records.len(), 1);
    }

    /// : `capture_mode` reflects the configured capture string.
    #[test]
    fn capture_mode_reflects_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let metadata_store = DiagnosticsStore::with_paths(
            DiagnosticsConfig::default(),
            dir.path().join("meta.jsonl"),
        )
        .expect("store");
        assert_eq!(metadata_store.capture_mode(), CaptureMode::Metadata);

        let full_store = DiagnosticsStore::with_paths(
            DiagnosticsConfig {
                capture: "full".to_string(),
                ..Default::default()
            },
            dir.path().join("full.jsonl"),
        )
        .expect("store");
        assert_eq!(full_store.capture_mode(), CaptureMode::Full);
    }
}
