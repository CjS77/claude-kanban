//! `claude-kanban` — a local-only Kanban board for a project, shared between a human in a browser and Claude over MCP.
//!
//! One binary, two faces, one store:
//!   - `serve` runs the web UI and its JSON API for the human.
//!   - `mcp` runs a stdio MCP server so Claude can read and move the board.
//!
//! Both read and write `.kanban/board.json`. The network surface is small and enumerable: the Create PR button (explicit
//! click), the `poll_interval`-gated gh PR poll, and — when a project's `minesweeper` toggle is on — the delegation
//! egress in [`minesweeper`] (compile it out with `--no-default-features`). Everything else stays on the machine.
//!
//! This library target exists so integration tests can reach the modules; the binary in `main.rs` is a one-line shim over
//! [`cli::run`]. The important layering, enforced by the module tree:
//!
//! - [`store`] owns the files, the lock, the version counter, and validation. It knows nothing about HTTP or MCP.
//! - `ops` (the single write funnel) turns typed operations into store mutations. Every writer goes through it.
//! - `server` and `mcp` are thin faces over `ops` and the read model in [`store::derive`].
//! - `worktree` drives git; its board writes still go through `ops`.

pub mod cli;
pub mod config;
pub mod diff;
pub mod git;
pub mod land;
pub mod mcp;
pub mod minesweeper;
pub mod ops;
pub mod pr;
pub mod server;
pub mod store;
pub mod worktree;
