//! Streaming vs non-streaming request metrics .
//!
//! 8 process-level atomic counters: 2 categories (stream / non-stream) × 4 dimensions
//! (request count, response bytes, input tokens, output tokens).
//!
//! Per ADR-038 Q3 these are aggregate counters — no per-request breakdown, no persistence.
//! Output: tracing events (log-aggregatable, cross-restart faithful).
//!
//! `UsageRecord` schema is untouched (`git diff --stat src/usage_stats.rs` stays empty).

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Snapshot of all 8 streaming metrics (for serialization / tracing).
#[derive(Debug, Clone, Default, Serialize)]
#[allow(dead_code)] // used in tests via snapshot() method
pub struct StreamingMetricsSnapshot {
    pub stream_requests: u64,
    pub non_stream_requests: u64,
    pub stream_response_bytes: u64,
    pub non_stream_response_bytes: u64,
    pub stream_input_tokens: u64,
    pub non_stream_input_tokens: u64,
    pub stream_output_tokens: u64,
    pub non_stream_output_tokens: u64,
}

/// Process-level atomic counters for streaming vs non-streaming request metrics.
///
/// Cheap to clone (all fields are `Arc`-wrapped atomics internally via `#[derive(Clone)]`
/// on the outer struct — actually just `Arc`-free since `AtomicU64` doesn't impl `Clone`;
/// we wrap the whole thing in `Arc` at the `AppState` level).
#[derive(Debug)]
pub struct StreamingMetrics {
    stream_requests: AtomicU64,
    non_stream_requests: AtomicU64,
    stream_response_bytes: AtomicU64,
    non_stream_response_bytes: AtomicU64,
    stream_input_tokens: AtomicU64,
    non_stream_input_tokens: AtomicU64,
    stream_output_tokens: AtomicU64,
    non_stream_output_tokens: AtomicU64,
}

impl Default for StreamingMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingMetrics {
    pub fn new() -> Self {
        Self {
            stream_requests: AtomicU64::new(0),
            non_stream_requests: AtomicU64::new(0),
            stream_response_bytes: AtomicU64::new(0),
            non_stream_response_bytes: AtomicU64::new(0),
            stream_input_tokens: AtomicU64::new(0),
            non_stream_input_tokens: AtomicU64::new(0),
            stream_output_tokens: AtomicU64::new(0),
            non_stream_output_tokens: AtomicU64::new(0),
        }
    }

    /// Record a single request's metrics.
    ///
    /// Called from `AppState::record_usage_with_status` — the single funnel point
    /// where all forwarding paths (streaming and non-streaming) converge.
    ///
    /// Emits a tracing event at INFO level for log aggregation.
    pub fn record(
        &self,
        is_streaming: bool,
        response_bytes: u64,
        input_tokens: i64,
        output_tokens: i64,
    ) {
        let input = input_tokens.max(0) as u64;
        let output = output_tokens.max(0) as u64;

        if is_streaming {
            self.stream_requests.fetch_add(1, Ordering::Relaxed);
            self.stream_response_bytes
                .fetch_add(response_bytes, Ordering::Relaxed);
            self.stream_input_tokens.fetch_add(input, Ordering::Relaxed);
            self.stream_output_tokens
                .fetch_add(output, Ordering::Relaxed);
        } else {
            self.non_stream_requests.fetch_add(1, Ordering::Relaxed);
            self.non_stream_response_bytes
                .fetch_add(response_bytes, Ordering::Relaxed);
            self.non_stream_input_tokens
                .fetch_add(input, Ordering::Relaxed);
            self.non_stream_output_tokens
                .fetch_add(output, Ordering::Relaxed);
        }

        tracing::info!(
            target: "llm_proxy::streaming_metrics",
            is_streaming,
            response_bytes,
            input_tokens = input,
            output_tokens = output,
            "streaming metric recorded"
        );
    }

    /// Take a point-in-time snapshot of all 8 counters.
    #[allow(dead_code)] // used in tests
    pub fn snapshot(&self) -> StreamingMetricsSnapshot {
        StreamingMetricsSnapshot {
            stream_requests: self.stream_requests.load(Ordering::Relaxed),
            non_stream_requests: self.non_stream_requests.load(Ordering::Relaxed),
            stream_response_bytes: self.stream_response_bytes.load(Ordering::Relaxed),
            non_stream_response_bytes: self.non_stream_response_bytes.load(Ordering::Relaxed),
            stream_input_tokens: self.stream_input_tokens.load(Ordering::Relaxed),
            non_stream_input_tokens: self.non_stream_input_tokens.load(Ordering::Relaxed),
            stream_output_tokens: self.stream_output_tokens.load(Ordering::Relaxed),
            non_stream_output_tokens: self.non_stream_output_tokens.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_metrics_are_zero() {
        let m = StreamingMetrics::new();
        let s = m.snapshot();
        assert_eq!(s.stream_requests, 0);
        assert_eq!(s.non_stream_requests, 0);
        assert_eq!(s.stream_response_bytes, 0);
        assert_eq!(s.non_stream_response_bytes, 0);
        assert_eq!(s.stream_input_tokens, 0);
        assert_eq!(s.non_stream_input_tokens, 0);
        assert_eq!(s.stream_output_tokens, 0);
        assert_eq!(s.non_stream_output_tokens, 0);
    }

    #[test]
    fn record_streaming_increments_stream_counters() {
        let m = StreamingMetrics::new();
        m.record(true, 1024, 100, 200);
        let s = m.snapshot();
        assert_eq!(s.stream_requests, 1);
        assert_eq!(s.stream_response_bytes, 1024);
        assert_eq!(s.stream_input_tokens, 100);
        assert_eq!(s.stream_output_tokens, 200);
        // non-stream untouched
        assert_eq!(s.non_stream_requests, 0);
        assert_eq!(s.non_stream_response_bytes, 0);
        assert_eq!(s.non_stream_input_tokens, 0);
        assert_eq!(s.non_stream_output_tokens, 0);
    }

    #[test]
    fn record_non_streaming_increments_non_stream_counters() {
        let m = StreamingMetrics::new();
        m.record(false, 512, 50, 75);
        let s = m.snapshot();
        assert_eq!(s.non_stream_requests, 1);
        assert_eq!(s.non_stream_response_bytes, 512);
        assert_eq!(s.non_stream_input_tokens, 50);
        assert_eq!(s.non_stream_output_tokens, 75);
        // stream untouched
        assert_eq!(s.stream_requests, 0);
        assert_eq!(s.stream_response_bytes, 0);
        assert_eq!(s.stream_input_tokens, 0);
        assert_eq!(s.stream_output_tokens, 0);
    }

    #[test]
    fn multiple_records_accumulate() {
        let m = StreamingMetrics::new();
        m.record(true, 100, 10, 20);
        m.record(true, 200, 30, 40);
        m.record(false, 50, 5, 10);
        let s = m.snapshot();
        assert_eq!(s.stream_requests, 2);
        assert_eq!(s.stream_response_bytes, 300);
        assert_eq!(s.stream_input_tokens, 40);
        assert_eq!(s.stream_output_tokens, 60);
        assert_eq!(s.non_stream_requests, 1);
        assert_eq!(s.non_stream_response_bytes, 50);
        assert_eq!(s.non_stream_input_tokens, 5);
        assert_eq!(s.non_stream_output_tokens, 10);
    }

    #[test]
    fn negative_tokens_clamped_to_zero() {
        let m = StreamingMetrics::new();
        m.record(true, 0, -10, -20);
        let s = m.snapshot();
        assert_eq!(s.stream_input_tokens, 0);
        assert_eq!(s.stream_output_tokens, 0);
    }

    /// Mutation test: if the streaming flag is swapped (true↔false),
    /// the counters must go to the wrong bucket — proving the branch matters.
    #[test]
    fn mutation_stream_flag_swap_routes_to_wrong_bucket() {
        let m = StreamingMetrics::new();
        // Simulate what would happen if the is_streaming flag were inverted:
        // a streaming request recorded as non-streaming.
        m.record(false, 1024, 100, 200); // should have been true
        let s = m.snapshot();
        // The non-stream bucket got it (wrong bucket)
        assert_eq!(s.non_stream_requests, 1);
        assert_eq!(s.non_stream_response_bytes, 1024);
        // The stream bucket is empty (proving the branch is load-bearing)
        assert_eq!(s.stream_requests, 0);
        assert_eq!(s.stream_response_bytes, 0);
    }
}
