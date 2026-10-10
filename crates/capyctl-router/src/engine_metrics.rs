//! SPEC §17 (owner decision 2026-10-09): every completed chat response carries
//! one normalized set of per-request figures, each with its source.
//!
//! | figure | vLLM 0.30 (`metrics`) | TensorFold 0.6 (`tensorfold`) | SGLang 0.5.21 | llama.cpp v0.6.0 (`timings`) | router |
//! |---|---|---|---|---|---|
//! | `ttft_ms` | `queue_time_ms` + `time_to_first_token_ms` | `time_to_first_token` | — | — | forward start to first generated text |
//! | `queue_ms` | `queue_time_ms` | — | — | — | — |
//! | `prefill_ms` | `time_to_first_token_ms` | `prefill_seconds` | — | `prompt_ms` | — |
//! | `decode_tokens_per_second` | 1000 / `mean_itl_ms` | `tokens_per_second` (positive only) | — | `predicted_per_second` (positive only) | (completion tokens − 1) / first generated text to last chunk |
//! | `cached_tokens` | `usage.prompt_tokens_details.cached_tokens` | the same | the same (`--enable-cache-report`) | the same | — |
//!
//! vLLM 0.31.0 (ADR 0017 §2) still reports cached tokens at the same usage
//! path, gated by the same `--enable-prompt-tokens-details` extra flag
//! (`vllm/entrypoints/openai/chat_completion/serving.py`
//! `_make_prompt_tokens_details`); its per-request `metrics` object, which
//! gained `speculative_decoding`, never carries them, so the figure maps
//! from usage on both the 0.30.0 and 0.31.0 shapes and stays absent when the
//! engine reported none.
//!
//! An engine figure wins; the router derives `ttft_ms` and
//! `decode_tokens_per_second` from its own clock only when the engine sent
//! none, and labels them `router`. A figure nobody measured is absent, never
//! zero. The engine's own fields are left as it sent them.
//!
//! A non-streaming answer carries the set in its body as `capyctl.metrics`; a
//! stream carries it as one SSE comment line (`: x-capyctl-metrics {...}`)
//! before `data: [DONE]`, beside the timing comment, which SSE clients ignore.

use serde_json::{json, Map, Value};

use crate::timing::RequestTiming;

/// The SSE comment name a stream's figures ride under.
pub const METRICS_COMMENT: &str = "x-capyctl-metrics";
/// The response-body key a non-streaming answer carries them under.
pub const BODY_KEY: &str = "capyctl";

/// What the engine itself reported about one request.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EngineReport {
    pub ttft_ms: Option<f64>,
    pub queue_ms: Option<f64>,
    pub prefill_ms: Option<f64>,
    pub decode_tokens_per_second: Option<f64>,
    pub cached_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// llama.cpp's `timings.predicted_n`: the completion count when `usage`
    /// carries none.
    pub predicted_tokens: Option<u64>,
}

/// A finite, non-negative number at `key`, or nothing.
fn measured(object: &Value, key: &str) -> Option<f64> {
    object
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

impl EngineReport {
    /// Whether one streamed chunk may carry anything [`EngineReport::observe`]
    /// reads. SGLang sends `"usage":null` on every chunk, which carries
    /// nothing, so most chunks are never parsed. llama.cpp v0.6.0 puts
    /// `timings` on its last chunk (the usage chunk, else the finish chunk).
    pub fn may_carry(chunk: &str) -> bool {
        chunk.contains("\"tensorfold\"")
            || chunk.contains("\"metrics\"")
            || chunk.contains("\"timings\"")
            || (chunk.contains("\"usage\"")
                && !chunk.contains("\"usage\":null")
                && !chunk.contains("\"usage\": null"))
    }

    /// Fold in what one response or chunk reports. A later report of a
    /// figure replaces an earlier one; an absent one leaves it.
    pub fn observe(&mut self, value: &Value) {
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            if let Some(cached) = usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
            {
                self.cached_tokens = Some(cached);
            }
            if let Some(tokens) = usage.get("completion_tokens").and_then(Value::as_u64) {
                self.completion_tokens = Some(tokens);
            }
        }
        // vLLM 0.30 `PerRequestMetrics` (`--enable-per-request-metrics`,
        // generate/base/serving.py `build_per_request_timing_metrics`): its
        // `time_to_first_token_ms` runs from scheduling to the first token,
        // which is the prefill; the queue precedes it.
        if let Some(metrics) = value.get("metrics").filter(|m| m.is_object()) {
            let queue = measured(metrics, "queue_time_ms");
            let prefill = measured(metrics, "time_to_first_token_ms");
            if let (Some(queue), Some(prefill)) = (queue, prefill) {
                self.ttft_ms = Some(queue + prefill);
            }
            if queue.is_some() {
                self.queue_ms = queue;
            }
            if prefill.is_some() {
                self.prefill_ms = prefill;
            }
            if let Some(itl) = measured(metrics, "mean_itl_ms").filter(|itl| *itl > 0.0) {
                self.decode_tokens_per_second = Some(1000.0 / itl);
            }
        }
        // TensorFold 0.6.x run statistics (`server/app.py` `runtime`), in
        // seconds. Its `tokens_per_second` is 0.0 when it timed no decode,
        // which is unknown rather than zero.
        if let Some(stats) = value.get("tensorfold").filter(|s| s.is_object()) {
            if let Some(ttft) = measured(stats, "time_to_first_token") {
                self.ttft_ms = Some(ttft * 1000.0);
            }
            if let Some(prefill) = measured(stats, "prefill_seconds") {
                self.prefill_ms = Some(prefill * 1000.0);
            }
            if let Some(rate) = measured(stats, "tokens_per_second").filter(|r| *r > 0.0) {
                self.decode_tokens_per_second = Some(rate);
            }
        }
        // ADR 0029 §11: llama.cpp v0.6.0 `timings`
        // (`tools/server/server-common.cpp` `server_slot_stats::to_json`), in
        // milliseconds. `predicted_per_second` is 0 or not finite when no
        // decode was timed, which is unknown rather than zero.
        if let Some(timings) = value.get("timings").filter(|t| t.is_object()) {
            if let Some(prefill) = measured(timings, "prompt_ms") {
                self.prefill_ms = Some(prefill);
            }
            if let Some(rate) = measured(timings, "predicted_per_second").filter(|r| *r > 0.0) {
                self.decode_tokens_per_second = Some(rate);
            }
            if let Some(tokens) = timings.get("predicted_n").and_then(Value::as_u64) {
                self.predicted_tokens = Some(tokens);
            }
        }
    }
}

fn rounded(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// The request's figures: the engine's where it reported them, the router's
/// where only its clock measured them, and nothing else.
pub fn metrics(report: &EngineReport, timing: &RequestTiming) -> Value {
    let mut out = Map::new();
    let mut put = |name: &str, engine: Option<f64>, router: Option<f64>| {
        let figure = engine
            .map(|value| (value, "engine"))
            .or_else(|| router.map(|value| (value, "router")));
        if let Some((value, source)) = figure {
            out.insert(
                name.to_owned(),
                json!({"value": rounded(value), "source": source}),
            );
        }
    };
    let router_ttft = timing
        .upstream_first_content()
        .map(|took| took.as_secs_f64() * 1000.0);
    // The first token is the one that opened the span; the rest were decoded
    // within it.
    let completion = report.completion_tokens.or(report.predicted_tokens);
    let router_decode = match (completion, timing.generating_span()) {
        (Some(tokens), Some(span)) if tokens >= 2 && !span.is_zero() => {
            Some((tokens - 1) as f64 / span.as_secs_f64())
        }
        _ => None,
    };
    put("ttft_ms", report.ttft_ms, router_ttft);
    put("queue_ms", report.queue_ms, None);
    put("prefill_ms", report.prefill_ms, None);
    put(
        "decode_tokens_per_second",
        report.decode_tokens_per_second,
        router_decode,
    );
    if let Some(cached) = report.cached_tokens {
        out.insert(
            "cached_tokens".to_owned(),
            json!({"value": cached, "source": "engine"}),
        );
    }
    Value::Object(out)
}

/// A non-streaming answer: its figures go in the body as `capyctl.metrics`.
pub fn attach(response: &mut Value, timing: &RequestTiming) {
    let mut report = EngineReport::default();
    report.observe(response);
    let figures = metrics(&report, timing);
    if let Some(body) = response.as_object_mut() {
        body.insert(BODY_KEY.to_owned(), json!({ "metrics": figures }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_only(response: &Value) -> Value {
        let mut report = EngineReport::default();
        report.observe(response);
        metrics(&report, &RequestTiming::untracked())
    }

    // T40: vLLM 0.30's non-streaming answer (`ChatCompletionResponse.model_dump`)
    // carries `metrics` with nulls for what it did not time.
    #[test]
    fn vllm_metrics_map_and_nulls_stay_absent() {
        let figures = engine_only(&json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                "logprobs": null, "finish_reason": "stop", "stop_reason": null}],
            "usage": {"prompt_tokens": 12, "total_tokens": 20, "completion_tokens": 8,
                "prompt_tokens_details": null},
            "prompt_logprobs": null, "kv_transfer_params": null,
            "metrics": {"time_to_first_token_ms": 41.5, "generation_time_ms": 70.0,
                "queue_time_ms": 0.75, "mean_itl_ms": 10.0, "tokens_per_second": 71.2,
                "speculative_decoding": null}
        }));
        assert_eq!(
            figures,
            json!({
                "ttft_ms": {"value": 42.25, "source": "engine"},
                "queue_ms": {"value": 0.75, "source": "engine"},
                "prefill_ms": {"value": 41.5, "source": "engine"},
                "decode_tokens_per_second": {"value": 100.0, "source": "engine"},
            })
        );
        // A one-token answer has no inter-token latency; no queue time means
        // no engine TTFT either. Absent, never zero.
        let figures = engine_only(&json!({"metrics": {"time_to_first_token_ms": 41.5,
            "queue_time_ms": null, "mean_itl_ms": null, "tokens_per_second": 24.1}}));
        assert_eq!(
            figures,
            json!({"prefill_ms": {"value": 41.5, "source": "engine"}})
        );
    }

    // T40: vLLM 0.31.0's answer keeps the same figures (raw
    // vllm-manual/chat-nonstream.json, engine-qual 2026-10-10): `metrics`
    // gained `speculative_decoding` and `usage.prompt_tokens_details` is
    // null without `--enable-prompt-tokens-details` (extra). The cached-token
    // figure maps from usage on that shape too and stays absent when the
    // engine reported none; the metrics object never carries it.
    #[test]
    fn vllm_0310_maps_cached_tokens_from_usage_or_leaves_it_absent() {
        let base = json!({
            "id": "chatcmpl-9aa8c1eaac803980", "object": "chat.completion",
            "created": 1791670426, "model": "frognano-nvfp4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": null,
                "refusal": null, "annotations": null, "audio": null,
                "function_call": null, "reasoning": "Thinking"}, "logprobs": null,
                "finish_reason": "length", "stop_reason": null, "token_ids": null,
                "routed_experts": null}],
            "service_tier": null, "system_fingerprint": "vllm-0.31.0-b6c14f9f",
            "usage": {"prompt_tokens": 16, "total_tokens": 64, "completion_tokens": 48,
                "prompt_tokens_details": {"cached_tokens": 16},
                "completion_tokens_details": {"reasoning_tokens": 48}},
            "prompt_logprobs": null,
            "metrics": {"time_to_first_token_ms": 41.2, "generation_time_ms": 371.7,
                "queue_time_ms": 0.026, "mean_itl_ms": 7.9, "tokens_per_second": 116.2,
                "speculative_decoding": null}
        });
        let figures = engine_only(&base);
        assert_eq!(
            figures["cached_tokens"],
            json!({"value": 16, "source": "engine"})
        );
        assert_eq!(
            figures["prefill_ms"],
            json!({"value": 41.2, "source": "engine"})
        );
        let mut uncached = base;
        uncached["usage"]["prompt_tokens_details"] = json!(null);
        let figures = engine_only(&uncached);
        assert!(figures.get("cached_tokens").is_none(), "{figures}");
    }

    // T40 T41: TensorFold 0.6.5's `tensorfold` statistics, in seconds; its
    // 0.0 tokens per second (no decode timed) and null prefill are unknown.
    #[test]
    fn tensorfold_statistics_map_and_placeholders_stay_absent() {
        let figures = engine_only(&json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 0}},
            "exact_mode": "target-verified",
            "tensorfold": {"engine": "cuda", "tokens_per_second": 40.5, "seconds": 0.5,
                "prefill_seconds": 0.2, "time_to_first_token": 0.3}
        }));
        assert_eq!(
            figures,
            json!({
                "ttft_ms": {"value": 300.0, "source": "engine"},
                "prefill_ms": {"value": 200.0, "source": "engine"},
                "decode_tokens_per_second": {"value": 40.5, "source": "engine"},
                // TensorFold counts its prefix cache: zero is a measurement.
                "cached_tokens": {"value": 0, "source": "engine"},
            })
        );
        let figures = engine_only(&json!({"tensorfold": {"tokens_per_second": 0.0,
            "prefill_seconds": null, "time_to_first_token": null}}));
        assert_eq!(figures, json!({}));
        // The batched path reports no per-request timing at all.
        let figures = engine_only(&json!({"tensorfold": {"batch_size": 2, "seconds": 0.4}}));
        assert_eq!(figures, json!({}));
    }

    // T40: SGLang 0.5.21 reports only cached tokens, and only when some were
    // (`UsageProcessor._details_if_cached`); its timings are the router's.
    #[test]
    fn sglang_reports_cached_tokens_only() {
        let figures = engine_only(&json!({
            "usage": {"prompt_tokens": 12, "total_tokens": 20, "completion_tokens": 8,
                "prompt_tokens_details": {"cached_tokens": 8}, "reasoning_tokens": 0},
            "metadata": {"weight_version": "default"}
        }));
        assert_eq!(
            figures,
            json!({"cached_tokens": {"value": 8, "source": "engine"}})
        );
        let figures = engine_only(&json!({"usage": {"prompt_tokens": 12,
            "completion_tokens": 8, "prompt_tokens_details": null}}));
        assert_eq!(figures, json!({}));
    }

    // T40: chunks that carry nothing are not parsed; the ones that do are.
    #[test]
    fn only_chunks_that_may_report_are_read() {
        assert!(!EngineReport::may_carry(
            r#"{"choices":[{"index":0,"delta":{"content":"Hi"}}],"usage":null}"#
        ));
        assert!(!EngineReport::may_carry(
            r#"{"choices":[{"index":0,"delta":{"content":"say \"usage\""}}]}"#
        ));
        assert!(EngineReport::may_carry(
            r#"{"choices":[],"usage":{"completion_tokens":8}}"#
        ));
        assert!(EngineReport::may_carry(
            r#"{"choices": [], "usage": {"completion_tokens": 8}}"#
        ));
        assert!(EngineReport::may_carry(r#"{"choices":[],"tensorfold":{}}"#));
        assert!(EngineReport::may_carry(r#"{"choices":[],"metrics":{}}"#));
        assert!(EngineReport::may_carry(r#"{"choices":[],"timings":{}}"#));
        assert!(!EngineReport::may_carry(
            r#"{"choices":[{"index":0,"delta":{"content":"say \"timings\""}}]}"#
        ));
    }

    /// llama.cpp v0.6.0 `server_slot_stats::to_json` as one answer carries it.
    fn llamacpp_timings(predicted_per_second: f64) -> Value {
        json!({"cache_n": 8, "prompt_n": 4, "prompt_ms": 12.5,
            "prompt_per_token_ms": 3.125, "prompt_per_second": 320.0,
            "predicted_n": 6, "predicted_ms": 100.0, "predicted_per_token_ms": 16.667,
            "predicted_per_second": predicted_per_second})
    }

    fn llamacpp_usage() -> Value {
        json!({"completion_tokens": 6, "prompt_tokens": 12, "total_tokens": 18,
            "prompt_tokens_details": {"cached_tokens": 8}})
    }

    /// The router clock of a forward whose first text arrived after a pause.
    fn router_clock() -> RequestTiming {
        let mut timing = RequestTiming::untracked();
        timing.forwarding(Some(0), Some(1));
        std::thread::sleep(std::time::Duration::from_millis(2));
        timing.chunk(true);
        timing
    }

    fn llamacpp_set(ttft: &Value) -> Value {
        json!({
            "ttft_ms": ttft,
            "prefill_ms": {"value": 12.5, "source": "engine"},
            "decode_tokens_per_second": {"value": 60.0, "source": "engine"},
            "cached_tokens": {"value": 8, "source": "engine"},
        })
    }

    // T40 T42, ADR 0029 §11: a llama-server v0.6.0 answer
    // (`to_json_oaicompat_chat`): `prefill_ms` from `timings.prompt_ms`, the
    // decode rate from `predicted_per_second`, cached tokens from `usage`, the
    // router's `ttft_ms` and no `queue_ms`.
    #[test]
    fn llamacpp_answer_maps_timings_and_usage() {
        let mut answer = json!({
            "choices": [{"finish_reason": "stop", "index": 0,
                "message": {"role": "assistant", "content": "hi"}}],
            "created": 1, "model": "m", "system_fingerprint": "b1-abc",
            "object": "chat.completion", "usage": llamacpp_usage(), "id": "chatcmpl-1",
            "timings": llamacpp_timings(60.0)
        });
        let engine = answer.clone();
        let timing = router_clock();
        attach(&mut answer, &timing);
        let figures = &answer["capyctl"]["metrics"];
        assert_eq!(figures["ttft_ms"]["source"], "router");
        assert_eq!(*figures, llamacpp_set(&figures["ttft_ms"]));
        assert!(figures.get("queue_ms").is_none());
        // The engine's own fields are relayed as it sent them.
        for key in ["choices", "usage", "timings", "system_fingerprint"] {
            assert_eq!(answer[key], engine[key]);
        }
        // A zero or missing decode rate is unknown, not zero.
        let figures = engine_only(&json!({"timings": {"prompt_ms": 12.5,
            "predicted_n": 0, "predicted_ms": 0.0, "predicted_per_second": 0.0}}));
        assert_eq!(
            figures,
            json!({"prefill_ms": {"value": 12.5, "source": "engine"}})
        );
        let figures = engine_only(&json!({"timings": {"prompt_ms": null}}));
        assert_eq!(figures, json!({}));
    }

    /// Fold a stream's chunks as the relay does: only those that may carry a
    /// report are parsed.
    fn streamed(chunks: &[Value]) -> (EngineReport, usize) {
        let mut report = EngineReport::default();
        let mut parsed = 0;
        for chunk in chunks {
            let text = chunk.to_string();
            if EngineReport::may_carry(&text) {
                parsed += 1;
                report.observe(&serde_json::from_str(&text).unwrap());
            }
        }
        (report, parsed)
    }

    fn llamacpp_chunk(delta: Value, finish: Value) -> Value {
        json!({"choices": [{"finish_reason": finish, "index": 0, "delta": delta}],
            "created": 1, "id": "chatcmpl-1", "model": "m", "system_fingerprint": "b1-abc",
            "object": "chat.completion.chunk"})
    }

    // T40 T42, ADR 0029 §11: a llama-server v0.6.0 stream
    // (`to_json_oaicompat_chat_stream`) puts `timings` on its last chunk: the
    // empty-`choices` usage chunk with `include_usage`, else the finish chunk.
    // Both yield the same set; chunks with neither are not parsed.
    #[test]
    fn llamacpp_stream_maps_the_last_chunk() {
        let opening = llamacpp_chunk(json!({"role": "assistant", "content": null}), Value::Null);
        let text = llamacpp_chunk(json!({"content": "hi"}), Value::Null);
        let finish = llamacpp_chunk(json!({}), json!("stop"));
        let mut usage = json!({"choices": [], "created": 1, "id": "chatcmpl-1", "model": "m",
            "system_fingerprint": "b1-abc", "object": "chat.completion.chunk",
            "usage": llamacpp_usage()});
        usage["timings"] = llamacpp_timings(60.0);
        let timing = router_clock();
        let (report, parsed) = streamed(&[opening.clone(), text.clone(), finish.clone(), usage]);
        assert_eq!(parsed, 1);
        let with_usage = metrics(&report, &timing);
        assert_eq!(with_usage["ttft_ms"]["source"], "router");
        assert_eq!(with_usage, llamacpp_set(&with_usage["ttft_ms"]));

        // Without `include_usage` the finish chunk carries `timings` alone:
        // no `usage`, so no cached tokens, and `predicted_n` is the count.
        let mut finish_timed = finish;
        finish_timed["timings"] = llamacpp_timings(60.0);
        let (report, parsed) = streamed(&[opening, text, finish_timed]);
        assert_eq!(parsed, 1);
        assert_eq!(report.completion_tokens, None);
        assert_eq!(report.predicted_tokens, Some(6));
        let figures = metrics(&report, &timing);
        let mut expected = llamacpp_set(&figures["ttft_ms"]);
        expected.as_object_mut().unwrap().remove("cached_tokens");
        assert_eq!(figures, expected);
    }

    // T40 T42: with no engine rate, the router's decode rate counts llama.cpp's
    // `predicted_n` when `usage` names no completion count.
    #[test]
    fn llamacpp_predicted_count_feeds_the_router_rate() {
        let mut timing = router_clock();
        std::thread::sleep(std::time::Duration::from_millis(2));
        timing.chunk(false);
        let report = EngineReport {
            predicted_tokens: Some(6),
            ..EngineReport::default()
        };
        assert_eq!(
            metrics(&report, &timing)["decode_tokens_per_second"]["source"],
            "router"
        );
        let report = EngineReport {
            predicted_tokens: Some(6),
            completion_tokens: Some(1),
            ..EngineReport::default()
        };
        assert!(metrics(&report, &timing)
            .get("decode_tokens_per_second")
            .is_none());
    }

    // T40: a non-streaming answer keeps the engine's fields and gains
    // `capyctl.metrics`, empty when nothing was measured.
    #[test]
    fn attach_leaves_the_engine_fields() {
        let engine = json!({"choices": [], "metrics": {"queue_time_ms": 1.0},
            "tensorfold": {"seconds": 1.0}});
        let mut answer = engine.clone();
        attach(&mut answer, &RequestTiming::untracked());
        for key in ["choices", "metrics", "tensorfold"] {
            assert_eq!(answer[key], engine[key]);
        }
        assert_eq!(
            answer["capyctl"]["metrics"],
            json!({"queue_ms": {"value": 1.0, "source": "engine"}})
        );
    }
}
