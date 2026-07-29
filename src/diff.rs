//! The View diff button's data: a review branch's own changes against main, shaped into a model the template renders
//! GitHub-style. Purely local — unlike [`crate::pr`] it never touches the network, needs no remote, and is offered on
//! any review ticket whose branch still exists (seeing the changes without pushing is the whole point).
//!
//! Git computes the diff (`git diff <main>...<branch>`, see [`crate::git::diff`]); [`unidiff`] parses its unified output
//! into files, hunks and lines carrying source/target line numbers. This module only reshapes that into a view-model —
//! the HTML lives in `templates/diff.html`, and syntax highlighting is the browser's job (highlight.js over the
//! `language-*` class each line carries), so nothing here builds markup.

use anyhow::Context;

use crate::{
    config::Config,
    git,
    store::{
        Store,
        model::{Column, Ticket, TicketId},
    },
    worktree,
};

/// One changed file: its path, how it changed, the +/- tallies, the highlight.js language for its extension, and its
/// hunks. A binary or pure-rename file carries no hunks and renders as its header row alone.
#[derive(Debug)]
pub struct FileDiff {
    pub path: String,
    /// The former path, present only on a rename.
    pub old_path: Option<String>,
    /// `added` / `modified` / `deleted` / `renamed` — also the status pill's text and its `diff-status-*` modifier.
    pub status: &'static str,
    pub added: usize,
    pub deleted: usize,
    /// The highlight.js language name for the file's extension, or `None` when unknown (rendered without a language, so
    /// highlight.js leaves the code plain).
    pub lang: Option<&'static str>,
    pub hunks: Vec<DiffHunk>,
}

/// One `@@` hunk: its section header (shown greyed, like GitHub) and its lines.
#[derive(Debug)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// One rendered line: the old/new line numbers (each absent on the side without it), the row's CSS modifier, and the
/// text with its diff marker already stripped.
#[derive(Debug)]
pub struct DiffLine {
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    /// `dl-add` / `dl-del` / `dl-ctx` — drives the red/green row tint in `assets/diff.css`.
    pub css: &'static str,
    pub text: String,
}

/// The button-visibility predicate: a non-external `review` ticket whose branch still exists locally. This is
/// [`crate::pr::eligible`] *without* the remote requirement — a diff is computed entirely locally, so it is offered even
/// where there is no remote to push to. Checked live per detail-pane render, never cached (one subprocess is cheap).
#[must_use]
pub fn eligible(store: &Store, ticket: &Ticket) -> bool {
    let Column::Review { branch: Some(branch) } = &ticket.column else { return false };
    if ticket.external.is_some() {
        return false; // external tickets are worked elsewhere; their branch may not be on this machine at all
    }
    let Ok(repo) = worktree::repo_root(store) else { return false };
    git::branch_exists(&repo, branch)
}

/// A computed diff: the resolved branch and main names (for the pane's header) and the per-file changes. An empty
/// `files` means the branch changes nothing; the template renders that as a friendly note.
#[derive(Debug)]
pub struct Diff {
    pub branch: String,
    pub main: String,
    pub files: Vec<FileDiff>,
}

/// Parse the review ticket's branch diff against main into a per-file model. Re-validates what [`eligible`] checks — the
/// render is stale by click time — and errors loudly (the handler turns that into a toast) rather than silently showing
/// nothing.
pub fn compute(store: &Store, id: &TicketId) -> anyhow::Result<Diff> {
    let repo = worktree::repo_root(store)?;
    let board = store.read_board()?;
    let ticket = board.ticket(id).with_context(|| format!("{id} not found on the board"))?;
    let Column::Review { branch: Some(branch) } = &ticket.column else {
        anyhow::bail!("{id} is not a review ticket with a branch — nothing to diff");
    };
    if !git::branch_exists(&repo, branch) {
        anyhow::bail!("branch {branch} no longer exists locally — already merged and deleted?");
    }
    let main = Config::load(store.dir())?
        .main_branch(&repo)
        .context("no main branch is configured or detectable to diff against")?;

    let files = parse(&git::diff(&repo, &main, branch)?)?;
    Ok(Diff { branch: branch.clone(), main, files })
}

/// Turn a unified diff into the view-model. Split from [`compute`] so it can be unit-tested against a literal patch with
/// no git repository in the loop.
fn parse(raw: &str) -> anyhow::Result<Vec<FileDiff>> {
    let mut patch = unidiff::PatchSet::new();
    patch.parse(raw).context("parsing the unified diff")?;
    Ok(patch.files().iter().map(file_diff).collect())
}

fn file_diff(pf: &unidiff::PatchedFile) -> FileDiff {
    let old = strip_prefix(&pf.source_file);
    let new = strip_prefix(&pf.target_file);
    let (status, path, old_path) = if pf.is_added_file() {
        ("added", new.to_owned(), None)
    } else if pf.is_removed_file() {
        ("deleted", old.to_owned(), None)
    } else if old != new {
        ("renamed", new.to_owned(), Some(old.to_owned()))
    } else {
        ("modified", new.to_owned(), None)
    };
    FileDiff {
        lang: lang_for(&path),
        path,
        old_path,
        status,
        added: pf.added(),
        deleted: pf.removed(),
        hunks: pf.hunks().iter().map(hunk).collect(),
    }
}

fn hunk(h: &unidiff::Hunk) -> DiffHunk {
    let ranges = format!("@@ -{},{} +{},{} @@", h.source_start, h.source_length, h.target_start, h.target_length);
    let header = if h.section_header.is_empty() { ranges } else { format!("{ranges} {}", h.section_header) };
    let lines = h
        .lines()
        .iter()
        .map(|l| DiffLine {
            old_no: l.source_line_no,
            new_no: l.target_line_no,
            css: if l.is_added() {
                "dl-add"
            } else if l.is_removed() {
                "dl-del"
            } else {
                "dl-ctx"
            },
            text: l.value.trim_end_matches(['\n', '\r']).to_owned(),
        })
        .collect();
    DiffHunk { header, lines }
}

/// Drop git's `a/` or `b/` diff prefix; `/dev/null` (the absent side of an add or delete) becomes empty.
fn strip_prefix(file: &str) -> &str {
    if file == "/dev/null" {
        return "";
    }
    file.strip_prefix("a/").or_else(|| file.strip_prefix("b/")).unwrap_or(file)
}

/// The highlight.js language for a path's extension, limited to what the vendored common build ships. `None` — an
/// unknown or missing extension — renders the code without a language class, so highlight.js leaves it plain.
fn lang_for(path: &str) -> Option<&'static str> {
    let ext = path.rsplit_once('.').map(|(_, e)| e)?;
    Some(match ext {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "php" => "php",
        "cs" => "csharp",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "sh" | "bash" | "zsh" => "bash",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "ini",
        "md" | "markdown" => "markdown",
        "html" | "htm" | "xml" | "svg" => "xml",
        "css" => "css",
        "scss" | "sass" => "scss",
        "sql" => "sql",
        "lua" => "lua",
        "diff" | "patch" => "diff",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODIFY: &str = "\
diff --git a/src/foo.rs b/src/foo.rs
index e69de29..1111111 100644
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,3 +1,3 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
 }
";

    #[test]
    fn parses_a_single_file_modification_with_line_numbers() {
        let files = parse(MODIFY).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.path, "src/foo.rs");
        assert_eq!(f.old_path, None);
        assert_eq!(f.status, "modified");
        assert_eq!(f.lang, Some("rust"));
        assert_eq!((f.added, f.deleted), (1, 1));

        let lines = &f.hunks[0].lines;
        let added = lines.iter().find(|l| l.css == "dl-add").unwrap();
        let removed = lines.iter().find(|l| l.css == "dl-del").unwrap();
        // The diff marker is stripped, indentation is kept, and each side knows only its own line number.
        assert_eq!(added.text, "    println!(\"new\");");
        assert!(!added.text.starts_with('+'), "the +/- marker must not survive into the rendered text");
        assert!(added.new_no.is_some() && added.old_no.is_none());
        assert!(removed.old_no.is_some() && removed.new_no.is_none());
    }

    #[test]
    fn classifies_added_and_deleted_files() {
        let added = parse("--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hi\n").unwrap();
        assert_eq!((added[0].status, added[0].path.as_str()), ("added", "new.txt"));

        let deleted = parse("--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n").unwrap();
        assert_eq!((deleted[0].status, deleted[0].path.as_str()), ("deleted", "gone.txt"));
    }

    #[test]
    fn empty_diff_yields_no_files() {
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn language_is_mapped_by_extension_and_absent_when_unknown() {
        assert_eq!(lang_for("src/main.rs"), Some("rust"));
        assert_eq!(lang_for("web/app.ts"), Some("typescript"));
        assert_eq!(lang_for("Makefile"), None);
        assert_eq!(lang_for("notes"), None);
    }
}
