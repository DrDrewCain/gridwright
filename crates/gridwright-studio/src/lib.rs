//! An interactive shell over the gridwright engine.
//!
//! One application, two hosts. Natively it opens a window; on
//! `wasm32-unknown-unknown` it mounts to a `<canvas>` in a page. The same
//! `StudioApp` runs in both, and the only thing that differs is how a solve gets
//! off the interactive thread — see [`backend`].
//!
//! # Why this is stable Rust with no threads
//!
//! Everything here compiles on stable, single-threaded, with no atomics target
//! feature, no `-Z build-std`, no SharedArrayBuffer and therefore no COOP/COEP
//! requirement on whatever origin serves the page. That is a deliberate choice
//! rather than an unfinished one: parallelising the engine for the browser was
//! measured first and rayon turned out to touch about 0.2% of the interactive
//! loop. Two parts in a thousand does not justify a nightly toolchain, a custom
//! std build, and cross-origin isolation on the host — each of which is a thing
//! that can break a deployment, and all of which someone has to maintain.
//!
//! # Why the whole engine is here rather than behind a service
//!
//! `gridwright-worker` is an rlib as well as a cdylib, so reading a file and
//! solving a network are ordinary function calls. Natively that means no
//! serialisation at all. In a browser it means the file never leaves the tab,
//! which for grid data is frequently the difference between being allowed to
//! open it and not.

mod app;
mod backend;
mod layout;
mod theme;
mod view;

pub use app::StudioApp;
pub use backend::{DefaultSolver, SolveBackend};
pub use layout::layout;
pub use theme::apply as apply_theme;
pub use view::NetworkView;

/// The browser entry point.
///
/// Split from `main` because a wasm module has no `main` worth running: the page
/// decides when the canvas exists, so mounting is something JavaScript asks for
/// rather than something that happens on load.
#[cfg(target_arch = "wasm32")]
pub use web_entry::mount;

#[cfg(target_arch = "wasm32")]
mod web_entry {
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::*;

    /// Mount the studio onto the canvas with the given element id.
    ///
    /// Returns immediately. eframe's start-up is async — it has to negotiate a
    /// WebGL context — and a `#[wasm_bindgen]` export cannot be, so the future
    /// is handed to the browser's microtask queue and the errors it can produce
    /// are reported to the console rather than lost.
    #[wasm_bindgen]
    pub fn mount(canvas_id: &str) -> Result<(), JsValue> {
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .ok_or_else(|| JsValue::from_str("no document"))?
            .get_element_by_id(canvas_id)
            .ok_or_else(|| JsValue::from_str("no element with that id"))?
            .dyn_into::<web_sys::HtmlCanvasElement>()?;

        wasm_bindgen_futures::spawn_local(async move {
            let started = eframe::WebRunner::new()
                .start(
                    canvas,
                    eframe::WebOptions::default(),
                    Box::new(|cc| Ok(Box::new(crate::StudioApp::new(cc)))),
                )
                .await;

            if let Err(e) = started {
                web_sys::console::error_1(&e);
            }
        });

        Ok(())
    }
}
