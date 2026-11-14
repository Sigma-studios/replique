//! Stopping cleanly. On shutdown systemd sends SIGTERM to the whole unit, restic
//! included; Ctrl-C sends SIGINT to both as well. The client must outlive restic
//! to report the run as interrupted (spooled if the network is already down)
//! instead of leaving it for the server to declare lost, so it notes the signal
//! and lets restic stop first.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    pub fn install() -> Stop {
        let flag = Arc::new(AtomicBool::new(false));
        for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
            if let Err(e) = signal_hook::flag::register(sig, flag.clone()) {
                crate::log!("could not watch for signal {sig}: {e}");
            }
        }
        Stop(flag)
    }

    /// A flag nothing sets, for code paths that need one.
    pub fn never() -> Stop {
        Stop(Arc::new(AtomicBool::new(false)))
    }

    pub fn requested(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
