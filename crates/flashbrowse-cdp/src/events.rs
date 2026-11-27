//! Async CDP event dispatcher.
//!
//! The transport layer routes any frame containing `method` (and no `id`) to
//! [`EventEmitter::emit`]. Callers subscribe with [`EventEmitter::subscribe`]
//! to obtain a [`broadcast::Receiver`] for a given method name, e.g.
//! `Page.loadEventFired`.

use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use serde_json::Value;
use tokio::sync::broadcast;

/// Broadcast capacity per method. Events are fire-and-forget; slow receivers
/// that fall behind see `Lagged` and should re-sync from fresh state.
const CHANNEL_CAPACITY: usize = 256;

/// CDP event payload paired with its optional flattened `sessionId`.
#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub session_id: Option<String>,
    pub params: Value,
}

/// Routes CDP events to subscribers by method name.
///
/// `EventEmitter` is cheap to clone via `Arc`. Counters are lock-free
/// atomics so `emit` never stalls on a full lock under load (M13).
pub struct EventEmitter {
    subscribers: DashMap<String, Vec<broadcast::Sender<Value>>>,
    envelope_subscribers: DashMap<String, Vec<broadcast::Sender<EventEnvelope>>>,
    counts: DashMap<String, AtomicU64>,
}

impl EventEmitter {
    /// Create an empty emitter.
    pub fn new() -> Self {
        Self {
            subscribers: DashMap::new(),
            envelope_subscribers: DashMap::new(),
            counts: DashMap::new(),
        }
    }

    /// Subscribe to a CDP method, e.g. `"Page.loadEventFired"`.
    ///
    /// Multiple subscribers to the same method each get their own channel.
    pub fn subscribe(&self, method: &str) -> broadcast::Receiver<Value> {
        let (sender, receiver) = broadcast::channel(CHANNEL_CAPACITY);
        // Prune dead senders on subscribe too (not only on emit) so a
        // subscribe-heavy workload with no events cannot grow unboundedly.
        self.subscribers
            .entry(method.to_string())
            .or_default()
            .retain(|s| s.receiver_count() > 0);
        if let Some(mut senders) = self.subscribers.get_mut(method) {
            senders.push(sender);
        } else {
            self.subscribers.insert(method.to_string(), vec![sender]);
        }
        receiver
    }

    /// Subscribe to a CDP method with its flattened `sessionId` envelope.
    pub fn subscribe_envelopes(&self, method: &str) -> broadcast::Receiver<EventEnvelope> {
        let (sender, receiver) = broadcast::channel(CHANNEL_CAPACITY);
        self.envelope_subscribers
            .entry(method.to_string())
            .or_default()
            .retain(|s| s.receiver_count() > 0);
        if let Some(mut senders) = self.envelope_subscribers.get_mut(method) {
            senders.push(sender);
        } else {
            self.envelope_subscribers
                .insert(method.to_string(), vec![sender]);
        }
        receiver
    }

    /// Emit an event `params` payload to all subscribers of `method`.
    ///
    /// Dead senders (dropped receivers) are removed lazily.
    pub fn emit(&self, method: &str, params: Value) {
        self.emit_with_session(method, None, params);
    }

    /// Emit an event with its optional flattened `sessionId`.
    pub fn emit_with_session(&self, method: &str, session_id: Option<String>, params: Value) {
        if crate::transport::validate_json_depth(&params).is_err() {
            crate::transport::drop_json_iteratively(params);
            tracing::warn!(method, "discarded CDP event with excessive JSON nesting");
            return;
        }
        if let Some(mut senders) = self.subscribers.get_mut(method) {
            senders.retain(|s| s.receiver_count() > 0);
            for sender in senders.iter() {
                let _ = sender.send(params.clone());
            }
            if senders.is_empty() {
                drop(senders);
                self.subscribers.remove_if(method, |_, v| v.is_empty());
            }
        }
        if let Some(mut senders) = self.envelope_subscribers.get_mut(method) {
            senders.retain(|s| s.receiver_count() > 0);
            let env = EventEnvelope { session_id, params };
            for sender in senders.iter() {
                let _ = sender.send(env.clone());
            }
            if senders.is_empty() {
                drop(senders);
                self.envelope_subscribers
                    .remove_if(method, |_, v| v.is_empty());
            }
        }
        self.counts
            .entry(method.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Approximate number of events seen per method (for metrics/debug).
    pub fn event_count(&self, method: &str) -> u64 {
        self.counts
            .get(method)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

impl Default for EventEmitter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::EventEmitter;
    use serde_json::json;

    #[test]
    fn deeply_nested_event_is_discarded_without_recursive_drop() {
        let emitter = EventEmitter::new();
        let mut receiver = emitter.subscribe("deep");
        let value = (0..10_000).fold(serde_json::Value::Null, |value, _| {
            serde_json::Value::Array(vec![value])
        });
        emitter.emit("deep", value);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn session_envelope_preserves_session_id() {
        let emitter = EventEmitter::new();
        let mut raw_rx = emitter.subscribe("Runtime.consoleAPICalled");
        let mut env_rx = emitter.subscribe_envelopes("Runtime.consoleAPICalled");

        emitter.emit_with_session(
            "Runtime.consoleAPICalled",
            Some("sid-1".to_string()),
            json!({"type": "error"}),
        );
        emitter.emit_with_session(
            "Runtime.consoleAPICalled",
            Some("sid-2".to_string()),
            json!({"type": "log"}),
        );

        let e1 = env_rx.try_recv().expect("first envelope");
        let e2 = env_rx.try_recv().expect("second envelope");
        assert_eq!(e1.session_id.as_deref(), Some("sid-1"));
        assert_eq!(e1.params["type"], "error");
        assert_eq!(e2.session_id.as_deref(), Some("sid-2"));
        assert_eq!(e2.params["type"], "log");
        assert_eq!(raw_rx.try_recv().unwrap()["type"], "error");
        assert_eq!(raw_rx.try_recv().unwrap()["type"], "log");
    }
}
