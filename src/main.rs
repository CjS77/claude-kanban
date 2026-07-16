//! Binary entry point — everything lives in the library (see `lib.rs`); this is a shim so integration tests can share the
//! same code the binary runs.

fn main() -> anyhow::Result<()> {
    claude_kanban::cli::run()
}
