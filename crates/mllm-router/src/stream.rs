//! SSE streaming dispatch (F1 design §5, T17 groundwork): the router
//! forwards engine chunks as SSE `data:` frames, ends with `data: [DONE]`,
//! and releases the in-flight guard only on verified backend completion.
//! Client disconnects do NOT release accounting early (abandon semantics).

use std::convert::Infallible;
use std::sync::Arc;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;

use crate::admission::StaticStreamGuard;
use mllm_adapters::traits::StreamEnded;

const MAX_CHUNK_BYTES: usize = 64 * 1024;

pub fn stream_response(
    forward: Arc<dyn mllm_adapters::traits::ChatForward>,
    body: serde_json::Value,
    guard: StaticStreamGuard,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    let pump = tokio::spawn(async move {
        // Accounting was registered BEFORE stream_response (the caller
        // enforces the in-flight bound synchronously); the guard lives for
        // the whole backend stream: a client disconnect drops the SSE side
        // only — accounting stays conservative until the backend ends (F1
        // design §5: client disconnect is not proof the engine stopped).
        let guard = guard.abandon();
        // The legacy synchronous callback cannot await capacity. Fail the
        // delivery on overflow; never resume after a missing chunk and append
        // a successful terminal. The F2 cutover still needs an async bounded
        // sink and durable lease settlement instead of this process-local guard.
        let mut delivery_failed = false;
        let mut on_chunk = |chunk: String| {
            if delivery_failed {
                return;
            }
            if chunk.len() > MAX_CHUNK_BYTES
                || tx.try_send(Ok(Event::default().data(chunk))).is_err()
            {
                delivery_failed = true;
            }
        };
        let result = forward.forward_chat_stream(&body, &mut on_chunk).await;
        if matches!(result, Ok(StreamEnded::Completed)) {
            // Backend completion and downstream delivery are separate facts.
            guard.release();
            if !delivery_failed {
                // A full final queue does not silently lose the terminal. Bound
                // waiting for a stalled consumer after backend work has settled.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    tx.send(Ok(Event::default().data("[DONE]"))),
                )
                .await;
            }
        }
        // Errors, premature close, cancellation and panic retain the abandoned
        // charge. A transport end alone cannot establish backend quiescence.
        drop(tx);
    });
    let stream = async_stream::stream! {
        while let Some(ev) = rx.recv().await {
            yield ev;
        }
        let _ = pump.is_finished();
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}
