pub mod adoption;
pub mod guest;
pub mod host;
pub mod job_kind;
pub mod maintenance;
pub mod managed;
pub mod migration;
pub mod migration_transport;
pub mod persist;
pub mod physical_trial;
pub mod protocol;
pub mod rejection;
pub mod retention;
pub mod services;
pub mod shrink;
pub mod store;
pub mod transport;
pub mod vm_change;

pub use job_kind::JobKind;

/// Sends library diagnostics (iroh's connection and path logs) to stderr,
/// only when `RUST_LOG` is set, e.g. `RUST_LOG=iroh=debug`.
pub fn init_diagnostic_logging() {
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .try_init();
    }
}
