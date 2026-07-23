//! Debug-logging init for foreign (macOS / Windows / …) apps.
//!
//! The Rust side already sprinkles `tracing` calls — e.g.
//! `MemoryManager::save_session` logs `"Saved session '<id>'"` on every save
//! and `AppSession::run_agent` logs the auto-save warning — but a foreign
//! `.app` has no stderr sink, so without a subscriber those events are
//! dropped and are invisible to debugging. `init_oneai_log` installs a global
//! subscriber writing to `<log_dir>/oneai_rust.log` (line-buffered so it is
//! live-readable mid-stream). Foreign code calls it once at app start, next
//! to its own `StreamLog.start()`, so both logs land in the same dir.
//!
//! Idempotent: the first call wins; later calls (e.g. on `rebuildApp`) are
//! no-ops. `try_init` is used so a second subscriber never panics.

use std::fs::{File, OpenOptions};
use std::io::{LineWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

/// Owned writer that locks the shared line-buffered file per write. `Arc` clone
/// per `make_writer` is cheap; the mutex serializes concurrent threads so log
/// lines don't interleave. Holds an `Arc` (not a borrow) so the `Writer`
/// associated type can be `'static` — satisfying `MakeWriter`'s `Write + 'a`
/// bound for any `'a`.
struct ArcWriter(Arc<Mutex<LineWriter<File>>>);

impl Write for ArcWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut g = self.0.lock().map_err(|_| std::io::Error::other("rust log mutex poisoned"))?;
        g.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut g = self.0.lock().map_err(|_| std::io::Error::other("rust log mutex poisoned"))?;
        g.flush()
    }
}

struct FileMaker(Arc<Mutex<LineWriter<File>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for &'static FileMaker {
    type Writer = ArcWriter;
    fn make_writer(&'a self) -> Self::Writer {
        ArcWriter(self.0.clone())
    }
}

static INIT: OnceLock<()> = OnceLock::new();

/// Install a global `tracing` subscriber writing to `<log_dir>/oneai_rust.log`.
///
/// Foreign code (Swift `ensureApp`) calls this right after its own
/// `StreamLog.start()`, passing the same Application Support directory the
/// SQLite db and `oneai_stream.log` live in. The previous run's log is rolled
/// to `oneai_rust.log.prev` so the file stays bounded; `RUST_LOG` is honored
/// if set, otherwise the level defaults to `info`.
#[uniffi::export]
pub fn init_oneai_log(log_dir: String) {
    INIT.get_or_init(|| {
        let path = Path::new(&log_dir).join("oneai_rust.log");
        // Roll the previous log aside so this run starts fresh but the prior
        // run is still reachable for "it happened last time" comparisons.
        let _ = std::fs::rename(&path, path.with_extension("log.prev"));
        let file = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("init_oneai_log: cannot open {:?}: {}", path, e);
                return;
            }
        };
        // Leak intentionally: the subscriber is global and lives for the
        // process; the maker must be `'static` to satisfy `with_writer`.
        let maker: &'static FileMaker = Box::leak(Box::new(FileMaker(Arc::new(Mutex::new(
            LineWriter::new(file),
        )))));
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        // `try_init` sets the global default (visible on ALL threads incl.
        // tokio workers); errors only if another subscriber is already global,
        // which never happens here — ignore it regardless.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(maker)
            .with_ansi(false)
            .with_target(true)
            .try_init();
        tracing::info!("oneai rust log initialized at {}", path.display());
    });
}
