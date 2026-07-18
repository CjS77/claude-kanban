//! The command-line surface: argument types and the dispatcher.
//!
//! Note that `--store` has **no default**: when it is absent the store resolves through git to the main working tree's
//! `.kanban/` (see [`Store::resolve`]). Giving it a default of `./.kanban` — as an early scaffold did — would defeat that
//! anchoring and split the board in two the moment a command ran inside a ticket worktree.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::store::{Store, model::TicketId};

/// A local-only Kanban board for planning and tracking work with Claude.
#[derive(Debug, Parser)]
#[command(name = "claude-kanban", version, about = "A local-only Kanban board for planning and tracking work with Claude")]
pub struct Cli {
    /// Directory holding `board.json`. Defaults to the main working tree's `.kanban` (asked of git), falling back to
    /// `./.kanban` outside a repository.
    #[arg(long, env = "KANBAN_STORE", global = true, value_name = "DIR")]
    pub store: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create `board.json` and seed an empty board.
    Init,

    /// Serve the interactive board UI on localhost.
    Serve {
        /// Port to bind: this flag, `KANBAN_PORT`, or config `port` — an explicit choice fails loudly when taken; 0 picks
        /// a free one. Omitted entirely: try 4747, and hunt for a free port when another project already holds it.
        #[arg(long, env = "KANBAN_PORT")]
        port: Option<u16>,

        /// Do not open a browser on start.
        #[arg(long)]
        no_open: bool,

        /// Serve web assets from a directory instead of the embedded copies — UI development only (edit, refresh, no rebuild).
        #[arg(long, value_name = "DIR")]
        assets_dir: Option<PathBuf>,
    },

    /// Run the stdio MCP server. Claude Code launches this; it is not meant to be run by hand.
    Mcp,

    /// The one-ticket-one-checkout lifecycle: start, finish, and list ticket worktrees.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// Create (or re-attach) the ticket's branch and sparse worktree, and stamp them onto the board. The ticket must be
    /// claimed (in `doing`) first. Idempotent: run it again after a /tmp wipe and it recovers.
    Start {
        /// The ticket id, e.g. K-7.
        ticket: String,

        /// Base ref for a fresh branch. Defaults to the main checkout's current HEAD.
        #[arg(long)]
        base: Option<String>,

        /// Branch slug override (`k-7/<slug>`), for when you can condense the title better than the default derivation.
        #[arg(long)]
        slug: Option<String>,

        /// Worktree root override; beats `KANBAN_WORKTREE_DIR`, which beats config `worktree_root`, which beats /tmp/claude-kanban.
        #[arg(long, env = "KANBAN_WORKTREE_DIR", value_name = "DIR")]
        dir: Option<PathBuf>,

        /// Take a full worktree instead of the sparse one that excludes `.kanban/`.
        #[arg(long)]
        no_sparse: bool,

        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Remove the ticket's worktree (refusing if dirty) and prune. The branch survives and is reported — integrating it
    /// is your explicit next step.
    Finish {
        /// The ticket id, e.g. K-7.
        ticket: String,

        /// Throw away uncommitted changes in the worktree instead of refusing.
        #[arg(long)]
        force_discard: bool,

        /// Also merge the ticket's branch into the main checkout's current branch, in one motion.
        #[arg(long)]
        merge: bool,

        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Ticket worktrees joined with their claims: ticket, branch, path, dirty / missing state.
    List {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Parse arguments and run the selected command.
pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing();
    let store = Store::resolve(cli.store);

    // One choke point for the on-disk schema upgrade, covering serve, mcp and the worktree CLI alike. `init` is exempt so
    // its contract stays exactly "seed a new store, never touch an existing one".
    if !matches!(cli.command, Command::Init) {
        store.upgrade()?;
    }

    match cli.command {
        Command::Init => init(&store),
        Command::Serve { port, no_open, assets_dir } => crate::server::serve(store, port, no_open, assets_dir),
        Command::Mcp => crate::mcp::run(store),
        Command::Worktree { command } => worktree(&store, command),
    }
}

/// Dispatch `worktree start/finish/list`, printing a human summary or `--json`.
fn worktree(store: &Store, command: WorktreeCommand) -> anyhow::Result<()> {
    use crate::worktree as wt;
    match command {
        WorktreeCommand::Start { ticket, base, slug, dir, no_sparse, json } => {
            let report = wt::start(store, &TicketId(ticket), &wt::StartOpts { base, slug, dir, no_sparse })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let how = if report.reattached { "re-attached" } else { "created" };
                println!("{} worktree for {}:", how, report.ticket);
                println!("  branch  {}", report.branch);
                println!("  path    {}{}", report.path.display(), if report.sparse { "  (sparse — no .kanban/)" } else { "" });
                report.warnings.iter().for_each(|w| println!("  warning: {w}"));
            }
            Ok(())
        }
        WorktreeCommand::Finish { ticket, force_discard, merge, json } => {
            let report = wt::finish(store, &TicketId(ticket), force_discard, merge)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                match &report.removed {
                    Some(path) => println!("removed the worktree at {}", path.display()),
                    None => println!("no live worktree to remove"),
                }
                match &report.branch {
                    Some(branch) if report.merged => println!("branch {branch} merged into the current branch — and kept"),
                    Some(branch) => println!("branch {branch} kept — merge or push it when you're ready"),
                    None => println!("no branch recorded for {}", report.ticket),
                }
            }
            Ok(())
        }
        WorktreeCommand::List { json } => {
            let rows = wt::list(store)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!("no ticket worktrees");
            } else {
                for r in &rows {
                    let state = match (r.missing, r.dirty) {
                        (true, _) => "missing — restore with `worktree start`",
                        (false, true) => "dirty",
                        (false, false) => "clean",
                    };
                    let agent = r.agent.as_deref().map(|a| format!("  [{a}]")).unwrap_or_default();
                    println!("{:<6} {:<32} {}  ({state}){agent}", r.ticket.as_deref().unwrap_or("—"), r.branch, r.path.display());
                }
            }
            Ok(())
        }
    }
}

/// Logs go to STDERR, always — the `mcp` subcommand owns stdout for the MCP protocol, and mixing a log line into a JSON-RPC
/// stream corrupts the session. Filter with `RUST_LOG` (default: this crate at info).
///
/// Level discipline: `info` = lifecycle milestones and applied ops; `warn` = refusals (stale versions, the security guard,
/// error toasts); `debug` = op payloads, store writes, SSE broadcasts; `trace` = every git invocation.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("claude_kanban=info,tower_http=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

/// `claude-kanban init` — seed an empty board, a default config, and the store-local gitignore.
fn init(store: &Store) -> anyhow::Result<()> {
    store.init()?;
    println!("Initialised an empty board at {}", store.board_path().display());
    println!(
        "Commit {}, {}, and {} — claims, locks, and pid files stay untracked.",
        store.board_path().display(),
        store.config_path().display(),
        store.dir().join(".gitignore").display()
    );
    Ok(())
}
