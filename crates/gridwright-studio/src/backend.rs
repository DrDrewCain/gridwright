//! Getting a solved network back without stopping the frame.
//!
//! The interesting constraint is the same on both targets and has nothing to do
//! with speed: a solve of a few thousand rows takes hundreds of milliseconds and
//! a large one takes seconds, and a UI that blocks for that is a UI that has
//! stopped responding. Natively that is a thread; in a browser it is a Web
//! Worker, because the main thread is the only thread and blocking it freezes
//! the tab.
//!
//! Those two are different enough — one moves a `Network` by move, the other by
//! JSON across a `postMessage` — that the app should not be written against
//! either. Hence a trait, whose whole shape is "ask, then keep asking whether
//! it is done", because that is the only shape the browser side can implement.
//!
//! Note what is *not* here: threads inside the solve. Parallelising the solver
//! for wasm needs the atomics target feature, which needs nightly and
//! `-Z build-std`, and shipping it needs SharedArrayBuffer and therefore
//! COOP/COEP headers on the hosting origin. That was measured before it was
//! rejected: rayon touches about 0.2% of the interactive loop, so the whole
//! chain of nightly, custom std and cross-origin isolation buys two parts in a
//! thousand. The engine stays single-threaded and stable-Rust everywhere.

use gridwright_net::Network;
use gridwright_worker::{Failure, Solved};

/// A source of solved networks.
pub trait SolveBackend {
    /// Whether this backend can actually run a solve.
    ///
    /// Exists so the UI can disable the control and say why, rather than offer
    /// a button that fails. A backend under construction is a normal state for
    /// this crate to be in, not an error condition to surface at the moment
    /// somebody clicks.
    fn is_ready(&self) -> bool;

    /// Begin a solve. Must return promptly; the caller is mid-frame.
    fn submit(&mut self, network: &Network);

    /// True between [`submit`](SolveBackend::submit) and the result appearing.
    fn is_busy(&self) -> bool;

    /// The result, once, when there is one. Never blocks.
    fn take_result(&mut self) -> Option<Result<Solved, Failure>>;
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::ThreadSolver as DefaultSolver;

#[cfg(target_arch = "wasm32")]
pub use web::WorkerSolver as DefaultSolver;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::sync::mpsc::{Receiver, TryRecvError, channel};

    use eframe::egui;
    use gridwright_net::Network;
    use gridwright_worker::{Failure, Solved};

    use super::SolveBackend;

    /// One `std::thread` per solve.
    ///
    /// A thread pool would be the reflex, and there is nothing for it to pool:
    /// solves are user-initiated, one at a time, seconds apart at best. Thread
    /// creation is microseconds against a solve of hundreds of milliseconds.
    pub struct ThreadSolver {
        /// Held to wake the UI when an answer lands. eframe repaints on input,
        /// so without this the result would sit in the channel until the user
        /// happened to move the mouse — which looks exactly like a hang.
        ctx: egui::Context,
        rx: Option<Receiver<Result<Solved, Failure>>>,
    }

    impl ThreadSolver {
        pub fn new(ctx: egui::Context) -> Self {
            Self { ctx, rx: None }
        }
    }

    impl SolveBackend for ThreadSolver {
        fn is_ready(&self) -> bool {
            true
        }

        fn submit(&mut self, network: &Network) {
            let (tx, rx) = channel();
            self.rx = Some(rx);

            // Cloned rather than shared behind a lock. The alternative is for
            // the UI to hold a network it cannot edit while a solve runs, and
            // the whole reason the solve is off-thread is so that the UI stays
            // usable during it.
            let net = network.clone();
            let ctx = self.ctx.clone();

            std::thread::spawn(move || {
                let out = gridwright_worker::solve(&net);
                // A send failure means the app dropped the receiver — a newer
                // solve was submitted, or the window closed. Both are fine and
                // neither is this thread's problem.
                let _ = tx.send(out);
                ctx.request_repaint();
            });
        }

        fn is_busy(&self) -> bool {
            self.rx.is_some()
        }

        fn take_result(&mut self) -> Option<Result<Solved, Failure>> {
            match self.rx.as_ref()?.try_recv() {
                Ok(out) => {
                    self.rx = None;
                    Some(out)
                }
                Err(TryRecvError::Empty) => None,
                // The sender is gone without having sent, which can only mean
                // the solve thread unwound. Reported rather than swallowed:
                // silently returning to idle would look like a solve that
                // produced nothing.
                Err(TryRecvError::Disconnected) => {
                    self.rx = None;
                    Some(Err(Failure {
                        kind: "solve".into(),
                        message: "the solver thread stopped without returning a result".into(),
                    }))
                }
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use gridwright_net::Network;
    use gridwright_worker::{Failure, Solved};

    use super::SolveBackend;

    /// The browser side, solved on the interactive thread.
    ///
    /// A previous version of this was a `todo!()` waiting on Web Worker glue,
    /// and that was the wrong call: `gridwright-worker` is an rlib as well as a
    /// cdylib, so `solve()` is an ordinary Rust function this crate can already
    /// see. Waiting for a `postMessage` protocol before solving anything meant
    /// shipping a browser build that could open a network and not do the one
    /// thing the network is for.
    ///
    /// **This blocks the frame.** That is stated plainly because it is a real
    /// limitation and not a rounding error: the pure-Rust simplex runs at about
    /// 1.3x its native time under wasm, measured, so a few hundred rows is
    /// imperceptible, a few thousand is a stutter, and ten thousand-plus will
    /// freeze the tab for seconds. The size guard below refuses the cases that
    /// would freeze rather than appearing to hang.
    ///
    /// The Worker remains the right end state, and the trait this implements is
    /// unchanged by it — `submit` then poll is exactly the shape a worker needs.
    /// What changes when it lands is that this struct stops blocking, not that
    /// anything above it is rewritten.
    #[derive(Default)]
    pub struct WorkerSolver {
        /// Set by `submit`, consumed by `take_result`. Deliberately not solved
        /// inside `submit`: deferring by one frame lets the UI paint its
        /// "solving" state before the thread stops answering, so a slow solve
        /// looks like work rather than like a hang.
        pending: Option<Network>,
    }

    /// Rows above which the in-page solve is refused rather than attempted.
    ///
    /// From the measured wasm ladder: 3,456 rows takes 0.75 s and 13,824 takes
    /// 14.4 s. Somewhere in between, "the tab is thinking" becomes "the tab is
    /// broken", and a tool that freezes for fifteen seconds with no way out has
    /// lied about what it can do. Eight thousand is the last rung that stays
    /// under about four seconds.
    const MAX_ROWS_IN_PAGE: usize = 8_000;

    impl WorkerSolver {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl SolveBackend for WorkerSolver {
        fn is_ready(&self) -> bool {
            true
        }

        fn submit(&mut self, network: &Network) {
            self.pending = Some(network.clone());
        }

        fn is_busy(&self) -> bool {
            self.pending.is_some()
        }

        fn take_result(&mut self) -> Option<Result<Solved, Failure>> {
            let network = self.pending.take()?;

            // Estimated from the model rather than measured by trying: the
            // point is to refuse before the freeze, and a refusal that only
            // arrives after the solve is not a refusal.
            let rows = gridwright_build::Lopf::row_counts(&network).total();
            if rows > MAX_ROWS_IN_PAGE {
                // Built as a literal rather than through `Failure::new`, which
                // is private to the worker crate. Its fields are public exactly
                // so a caller can report something the worker did not produce.
                return Some(Err(Failure {
                    kind: "size".into(),
                    message: format!(
                        "{rows} rows is past what this page solves inline \
                         ({MAX_ROWS_IN_PAGE} max). It would freeze the tab for \
                         several seconds with no way to stop it. Run it natively, \
                         or wait for the background solver."
                    ),
                }));
            }

            Some(gridwright_worker::solve(&network))
        }
    }
}
