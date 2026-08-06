//! Live turn events for the WebTool.
//!
//! [`protocol::Event`] already describes what one agent turn does — tokens,
//! tool calls starting and ending, notices, errors. Its module doc promises
//! three clients over one protocol; the TUI consumed it from day one and the
//! web UI never did, which is why WebTool answers arrive as a single block
//! after every tool has finished running.
//!
//! This carries that same event vocabulary to the browser over SSE, plus the
//! cancellation token that makes a long turn interruptible. The two travel
//! together deliberately: every place that wants to report progress is also a
//! place that should check whether the user has given up, and threading one
//! parameter through the tool loop instead of two means they cannot drift
//! apart.
//!
//! The sink is optional. The WhatsApp path runs the same tool loop with
//! nowhere to stream to, and must not pay for events nobody reads.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::protocol::Event;

/// Events the WebTool streams that have no equivalent in the shared protocol.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WebEvent {
    /// The authoritative final answer.
    ///
    /// Streamed tokens are a preview: the tool loop passes its last round
    /// through `enforce_final_answer`, which can rewrite claims the tool
    /// observations do not support. The browser must persist *this* text, not
    /// the tokens it accumulated, or the stored transcript disagrees with the
    /// consistency layer.
    Answer { text: String },
}

/// Where a turn reports progress, and how it learns it should stop.
#[derive(Clone)]
pub struct TurnStream {
    events: Option<mpsc::Sender<Value>>,
    cancel: CancellationToken,
}

impl TurnStream {
    /// A stream wired to a browser connection.
    pub fn new(cancel: CancellationToken) -> (Self, mpsc::Receiver<Value>) {
        // Bounded: a runaway producer must slow down rather than grow the
        // queue until the process dies. 256 is far more than a browser falls
        // behind by in practice.
        let (sender, receiver) = mpsc::channel(256);
        (
            Self {
                events: Some(sender),
                cancel,
            },
            receiver,
        )
    }

    /// A stream with nowhere to report and nothing to cancel it, for callers
    /// that run the tool loop outside a browser connection.
    pub fn detached() -> Self {
        Self {
            events: None,
            cancel: CancellationToken::new(),
        }
    }

    /// Keeps the cancellation token but drops the sink.
    ///
    /// Background subagents outlive the request that started them, so they
    /// must not hold a sender whose receiver is already gone.
    pub fn without_sink(&self) -> Self {
        Self {
            events: None,
            cancel: self.cancel.clone(),
        }
    }

    pub async fn emit(&self, event: Event) {
        self.send(serde_json::to_value(event).unwrap_or(Value::Null))
            .await;
    }

    pub async fn emit_web(&self, event: WebEvent) {
        self.send(serde_json::to_value(event).unwrap_or(Value::Null))
            .await;
    }

    async fn send(&self, payload: Value) {
        let Some(sender) = &self.events else {
            return;
        };
        // A disconnected browser is the normal end of a stream, not an error.
        let _ = sender.send(payload).await;
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

impl Default for TurnStream {
    fn default() -> Self {
        Self::detached()
    }
}

/// The turn currently running for each conversation, so that an interrupt
/// arriving on a separate HTTP request can find the right one to stop.
#[derive(Clone, Default)]
pub struct LiveTurns {
    inner: Arc<Mutex<HashMap<String, LiveTurn>>>,
    next_id: Arc<AtomicU64>,
}

struct LiveTurn {
    id: u64,
    cancel: CancellationToken,
}

/// Identifies one registration, so a finishing turn cannot deregister the turn
/// that replaced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnId(u64);

impl LiveTurns {
    /// Registers a turn, cancelling whatever was still running for the same
    /// conversation. A second stream on one chat means the user resent or
    /// reloaded; leaving the first alive would race two tool loops over the
    /// same workspace.
    pub async fn begin(&self, chat_id: &str) -> (TurnId, CancellationToken) {
        let id = TurnId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let cancel = CancellationToken::new();
        let previous = self.inner.lock().await.insert(
            chat_id.to_string(),
            LiveTurn {
                id: id.0,
                cancel: cancel.clone(),
            },
        );
        if let Some(previous) = previous {
            previous.cancel.cancel();
        }
        (id, cancel)
    }

    /// Returns false when there was nothing running to stop.
    pub async fn cancel(&self, chat_id: &str) -> bool {
        let guard = self.inner.lock().await;
        let Some(turn) = guard.get(chat_id) else {
            return false;
        };
        turn.cancel.cancel();
        true
    }

    pub async fn finish(&self, chat_id: &str, id: TurnId) {
        let mut guard = self.inner.lock().await;
        if guard.get(chat_id).is_some_and(|turn| turn.id == id.0) {
            guard.remove(chat_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn events_serialize_with_the_shared_protocol_tag() {
        let (turn, mut receiver) = TurnStream::new(CancellationToken::new());
        turn.emit(Event::Token {
            text: "salut".into(),
        })
        .await;
        turn.emit_web(WebEvent::Answer {
            text: "final".into(),
        })
        .await;

        let token = receiver.recv().await.unwrap();
        assert_eq!(token["event"], "token");
        assert_eq!(token["text"], "salut");

        let answer = receiver.recv().await.unwrap();
        assert_eq!(answer["event"], "answer");
        assert_eq!(answer["text"], "final");
    }

    #[tokio::test]
    async fn a_detached_stream_swallows_events_without_blocking() {
        let turn = TurnStream::detached();
        turn.emit(Event::Token { text: "x".into() }).await;
        assert!(!turn.is_cancelled());
    }

    #[tokio::test]
    async fn dropping_the_receiver_does_not_fail_the_turn() {
        let (turn, receiver) = TurnStream::new(CancellationToken::new());
        drop(receiver);
        // The browser closed the tab mid-turn; the loop must keep going so it
        // can persist what it already did.
        turn.emit(Event::Token { text: "x".into() }).await;
    }

    #[tokio::test]
    async fn cancellation_is_visible_through_the_stream() {
        let cancel = CancellationToken::new();
        let (turn, _receiver) = TurnStream::new(cancel.clone());
        assert!(!turn.is_cancelled());
        cancel.cancel();
        assert!(turn.is_cancelled());
    }

    #[tokio::test]
    async fn interrupt_reaches_the_running_turn() {
        let turns = LiveTurns::default();
        let (id, cancel) = turns.begin("chat-1").await;
        assert!(turns.cancel("chat-1").await);
        assert!(cancel.is_cancelled());
        turns.finish("chat-1", id).await;
        assert!(!turns.cancel("chat-1").await);
    }

    #[tokio::test]
    async fn a_second_turn_cancels_the_first_on_the_same_chat() {
        let turns = LiveTurns::default();
        let (_first_id, first) = turns.begin("chat-1").await;
        let (_second_id, second) = turns.begin("chat-1").await;
        assert!(first.is_cancelled(), "the replaced turn must stop");
        assert!(!second.is_cancelled());
    }

    #[tokio::test]
    async fn a_finishing_turn_does_not_deregister_its_replacement() {
        let turns = LiveTurns::default();
        let (first_id, _first) = turns.begin("chat-1").await;
        let (_second_id, second) = turns.begin("chat-1").await;

        // The first turn notices its cancellation and tidies up late.
        turns.finish("chat-1", first_id).await;

        assert!(
            turns.cancel("chat-1").await,
            "the live replacement must still be interruptible"
        );
        assert!(second.is_cancelled());
    }

    #[tokio::test]
    async fn interrupting_an_idle_chat_is_not_an_error() {
        let turns = LiveTurns::default();
        assert!(!turns.cancel("never-started").await);
    }
}
