//! The sh launcher's platform resolution. `bin/kanban-mcp` is sourced with the `KANBAN_MCP_NO_MAIN` hook and a stubbed
//! `uname`, so `release_target`/`release_archive` can be exercised without fetching, unpacking, building, or exec'ing
//! anything. Windows users are promised no Rust toolchain, so git-bash / MSYS2 / Cygwin must resolve the same
//! x86_64-pc-windows-msvc zip the .cmd/.ps1 shims install rather than falling through to cargo — these pin that.
#![cfg(unix)]

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

fn launcher() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bin/kanban-mcp")
}

/// A temp dir holding an executable `uname` stub that answers `-s`/`-m` from `$STUB_SYS`/`$STUB_MACHINE`. The returned
/// `TempDir` owns the directory; keep it alive for as long as the stub must stay on `PATH`.
fn uname_stub() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("uname");
    std::fs::write(&stub, "#!/bin/sh\ncase \"$1\" in\n-s) echo \"$STUB_SYS\" ;;\n-m) echo \"$STUB_MACHINE\" ;;\n*) echo \"$STUB_SYS\" ;;\nesac\n")
        .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// Source the launcher under the `KANBAN_MCP_NO_MAIN` hook with `uname` reporting (sys, machine), run `snippet` (e.g.
/// `release_target`), and return its trimmed stdout plus whether it exited zero. The stub dir is prepended to `PATH` so
/// the sourced script sees it in place of the real uname; the launcher runs `set -eu`, so a failing final command
/// (`release_target` with no published binary) surfaces as `success() == false` with empty stdout.
fn run(sys: &str, machine: &str, snippet: &str) -> (String, bool) {
    let stub = uname_stub();
    let path = format!("{}:{}", stub.path().display(), std::env::var("PATH").unwrap());
    let out = Command::new("sh")
        .args(["-c", &format!(". \"$1\"; {snippet}"), "_", launcher().to_str().unwrap()])
        .env("KANBAN_MCP_NO_MAIN", "1")
        .env("CLAUDE_PLUGIN_ROOT", env!("CARGO_MANIFEST_DIR"))
        .env("PATH", path)
        .env("STUB_SYS", sys)
        .env("STUB_MACHINE", machine)
        .output()
        .unwrap();
    (String::from_utf8(out.stdout).unwrap().trim().to_owned(), out.status.success())
}

#[test]
fn git_bash_flavours_resolve_the_windows_msvc_target() {
    for sys in ["MINGW64_NT-10.0", "MSYS_NT-10.0", "CYGWIN_NT-10.0"] {
        let (out, ok) = run(sys, "x86_64", "release_target");
        assert!(ok, "{sys}/x86_64 must resolve a prebuilt target, not fall through to cargo");
        assert_eq!(out, "x86_64-pc-windows-msvc", "{sys}/x86_64 must pin the published Windows triple");
    }
}

#[test]
fn existing_platforms_still_resolve() {
    let cases = [("Linux", "x86_64", "x86_64-unknown-linux-musl"), ("Darwin", "arm64", "aarch64-apple-darwin")];
    for (sys, machine, want) in cases {
        let (out, ok) = run(sys, machine, "release_target");
        assert!(ok, "{sys}/{machine} must still resolve");
        assert_eq!(out, want, "{sys}/{machine} target regressed");
    }
}

#[test]
fn unsupported_windows_arch_falls_through_to_cargo() {
    // Only x86_64 is published for Windows; an arm64 git-bash has no artefact and must fail so ensure_binary
    // reaches build_from_source, exactly as the .ps1 shim does.
    let (out, ok) = run("MINGW64_NT-10.0", "aarch64", "release_target");
    assert!(!ok, "an unpublished Windows arch must make release_target fail");
    assert_eq!(out, "", "a failing release_target must print nothing");
}

#[test]
fn windows_installs_the_exe_path_the_shim_uses() {
    // The Windows zip ships claude-kanban.exe and bin/kanban-mcp.ps1 installs target/release/claude-kanban.exe. The
    // sh launcher must converge on that same file: under git-bash both launchers can run on one machine, and an
    // extensionless name here would have each materialise its own copy and re-download over the other forever.
    let (bin, ok) = run("MINGW64_NT-10.0", "x86_64", "echo \"$bin\"");
    assert!(ok, "the launcher must resolve $bin on Windows");
    assert!(bin.ends_with("/target/release/claude-kanban.exe"), "Windows must install the .exe the shim installs, got {bin}");

    let (bin, ok) = run("Linux", "x86_64", "echo \"$bin\"");
    assert!(ok, "the launcher must resolve $bin on Linux");
    // A path ending in the extensionless name cannot also end in .exe, so this alone pins the non-Windows install.
    assert!(bin.ends_with("/target/release/claude-kanban"), "non-Windows must keep the extensionless binary, got {bin}");
}

#[test]
fn release_archive_is_zip_on_windows_and_tarball_elsewhere() {
    let (zip, ok) = run("Linux", "x86_64", "release_archive x86_64-pc-windows-msvc");
    assert!(ok, "release_archive must succeed for a windows target");
    assert_eq!(zip, "claude-kanban-x86_64-pc-windows-msvc.zip", "release.yml attaches a .zip on Windows");

    let (tar, ok) = run("Linux", "x86_64", "release_archive x86_64-unknown-linux-musl");
    assert!(ok, "release_archive must succeed for a non-windows target");
    assert_eq!(tar, "claude-kanban-x86_64-unknown-linux-musl.tar.gz", "release.yml attaches a .tar.gz elsewhere");
}
