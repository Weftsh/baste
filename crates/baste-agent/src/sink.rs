//! Where the agent sends protocol events.

use baste_protocol::Event;
use std::io::Write;
use std::sync::{mpsc, Arc, Mutex};

/// A thread-safe destination for events.
pub trait EventSink: Send + Sync {
    fn send(&self, event: Event);
}

/// Writes one JSON object per line to any writer (stdout, a vsock stream, ...).
pub struct JsonLinesSink<W: Write + Send> {
    out: Mutex<W>,
}

impl<W: Write + Send> JsonLinesSink<W> {
    pub fn new(out: W) -> Self {
        JsonLinesSink {
            out: Mutex::new(out),
        }
    }
}

impl<W: Write + Send> EventSink for JsonLinesSink<W> {
    fn send(&self, event: Event) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        // If the host went away there is nobody left to tell; keep running so
        // post steps and cleanup still happen.
        let _ = writeln!(out, "{}", event.to_line());
        let _ = out.flush();
    }
}

/// Collects events in memory; used by tests and in-process runners.
#[derive(Clone, Default)]
pub struct VecSink {
    pub events: Arc<Mutex<Vec<Event>>>,
}

impl EventSink for VecSink {
    fn send(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

/// Forwards events over a channel.
pub struct ChannelSink {
    tx: Mutex<mpsc::Sender<Event>>,
}

impl ChannelSink {
    pub fn new(tx: mpsc::Sender<Event>) -> Self {
        ChannelSink { tx: Mutex::new(tx) }
    }
}

impl EventSink for ChannelSink {
    fn send(&self, event: Event) {
        let _ = self.tx.lock().unwrap().send(event);
    }
}
