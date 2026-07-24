use futures::future::{join_all, BoxFuture};
use log::debug;
use serde_json::Value;
use std::{collections::HashMap, fmt::Debug};
use uuid::Uuid;

#[derive(Debug)]
pub enum Event {
    ControllerPaired { id: Uuid },
    ControllerUnpaired { id: Uuid },
    CharacteristicValueChanged { aid: u64, iid: u64, value: Value },
}

#[derive(Default)]
pub struct EventEmitter {
    listeners: HashMap<u64, Box<dyn (Fn(&Event) -> BoxFuture<()>) + Send + Sync>>,
    next_id: u64,
}

impl EventEmitter {
    pub fn new() -> EventEmitter {
        EventEmitter {
            listeners: HashMap::new(),
            next_id: 0,
        }
    }

    /// Registers a listener and returns a token that can later be passed to
    /// [`remove_listener`](EventEmitter::remove_listener) to deregister it.
    pub fn add_listener(&mut self, listener: Box<dyn (Fn(&Event) -> BoxFuture<()>) + Send + Sync>) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.listeners.insert(id, listener);
        id
    }

    /// Deregisters a previously added listener. Connection handlers call this
    /// when the connection closes so per-connection listeners don't accumulate
    /// for the entire lifetime of the server.
    pub fn remove_listener(&mut self, token: u64) { self.listeners.remove(&token); }

    pub async fn emit(&self, event: &Event) {
        debug!("emitting event: {:?}", event);

        join_all(self.listeners.values().map(|listener| listener(event))).await;
    }
}
