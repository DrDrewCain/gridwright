//! Native entry point.
//!
//! Thin on purpose: everything the browser also runs lives in the library, so
//! the only thing here is opening a window and, if a path was given, handing the
//! first file to the app before the first frame.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    use eframe::egui;

    // Taken before `run_native` because the closure below is `FnOnce` and moving
    // an owned `Option<String>` into it is simpler than borrowing argv from it.
    let path = std::env::args().nth(1);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            // Below roughly this the side panel and the canvas stop being two
            // usable regions and become one unusable one.
            .with_min_inner_size([720.0, 420.0])
            .with_title("gridwright studio"),
        ..Default::default()
    };

    eframe::run_native(
        "gridwright studio",
        options,
        Box::new(move |cc| {
            let mut app = gridwright_studio::StudioApp::new(cc);
            if let Some(path) = path
                && let Err(e) = open(&mut app, &path)
            {
                eprintln!("{path}: {e}");
            }
            Ok(Box::new(app))
        }),
    )
}

/// Open a file, or a directory of them.
///
/// A PyPSA network is a *directory* -- `buses.csv`, `lines.csv`, and one file
/// per time series -- so a studio that only accepts single files cannot open
/// the one format in the reader set that carries geography and a real horizon.
/// The browser has no filesystem and will always go through bytes, so this
/// lives here rather than in the library.
#[cfg(not(target_arch = "wasm32"))]
fn open(app: &mut gridwright_studio::StudioApp, path: &str) -> std::io::Result<()> {
    if std::fs::metadata(path)?.is_dir() {
        return match gridwright_io::load_network(std::path::Path::new(path)) {
            Ok(net) => {
                app.open_network(net, path);
                Ok(())
            }
            Err(e) => Err(std::io::Error::other(e.to_string())),
        };
    }
    // The file name is passed along because it is what format detection looks
    // at first; content sniffing only takes over when the name is absent or
    // unhelpful.
    app.open_bytes(Some(path), &std::fs::read(path)?);
    Ok(())
}

/// The wasm build compiles this file too — a `[[bin]]` target is not
/// target-conditional — and there is nothing for it to do. The real browser
/// entry point is `gridwright_studio::mount`, called from the page.
#[cfg(target_arch = "wasm32")]
fn main() {}
