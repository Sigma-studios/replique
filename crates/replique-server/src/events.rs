//! Live updates for open pages, over Server-Sent Events.
//!
//! Anything that changes what a page shows publishes an event here; `/events`
//! forwards them to every logged-in browser. Pages react by re-fetching the
//! fragment concerned (htmx) or appending a point to a chart (uPlot), so events
//! stay small and never carry anything secret.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures_util::Stream;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::App;
use crate::auth::Admin;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A run of this device started, progressed or finished.
    Runs { device_id: String },
    /// A device was added, paired, changed or removed.
    Devices,
    /// A new measurement: `scope` is `box` or `repo:<id>`.
    Measured { scope: String, at: i64, bytes: i64 },
    /// An alert opened or cleared.
    Problems,
}

impl Event {
    fn name(&self) -> &'static str {
        match self {
            Event::Runs { .. } => "runs",
            Event::Devices => "devices",
            Event::Measured { .. } => "measured",
            Event::Problems => "problems",
        }
    }
}

pub struct Hub(broadcast::Sender<Event>);

impl Default for Hub {
    fn default() -> Self {
        Hub(broadcast::channel(256).0)
    }
}

impl Hub {
    pub fn publish(&self, event: Event) {
        // No subscriber is not an error: nobody is looking.
        let _ = self.0.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.subscribe()
    }
}

/// `GET /events`.
pub async fn stream(
    State(app): State<Arc<App>>,
    _admin: Admin,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let events = BroadcastStream::new(app.hub.subscribe()).filter_map(|event| {
        // A lagging browser just misses some events; the next one refreshes it.
        let event = event.ok()?;
        let data = serde_json::to_string(&event).ok()?;
        Some(Ok(SseEvent::default().event(event.name()).data(data)))
    });
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}
