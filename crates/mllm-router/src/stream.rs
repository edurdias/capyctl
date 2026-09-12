//! SSE streaming dispatch (F1 design §5, T17 groundwork): the router
//! forwards engine chunks as SSE `data:` frames, ends with `data: [DONE]`,
//! and releases the in-flight guard only when the backend stream ends.
//! Client disconnects do NOT release accounting early (abandon semantics).

use std::convert::Infallible;
use std::sync::Arc;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;

use crate::admission::StaticStreamGuard;

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
        let mut on_chunk = |chunk: String| {
            let _ = tx.try_send(Ok(Event::default().data(chunk)));
        };
        let result = forward.forward_chat_stream(&body, &mut on_chunk).await;
        match result {
            Ok(_) => {
                let _ = tx.try_send(Ok(Event::default().data("[DONE]")));
            }
            Err(_) => {
                // Backend ended abnormally: close without a [DONE] marker —
                // never fabricate a successful end (design §5).
            }
        }
        // Backend stream ended (however it ended): release accounting.
        guard.release();
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