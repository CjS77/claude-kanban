# MCP entry point for the installed plugin on Windows, invoked by the kanban-mcp.cmd trampoline. Mirrors
# bin/kanban-mcp (sh): a fresh install has no target/, so on first launch fetch the prebuilt release binary
# for this plugin version, verify its checksum, and run it. cargo build --release remains the fallback
# (arm64, offline, verification failure), so no Rust toolchain is required on x86_64. stdout belongs to
# JSON-RPC -- one stray line corrupts the session, so every diagnostic goes to stderr, progress rendering is
# off, and the final exe launch hands over this process's raw stdio handles untouched.
# Kept ASCII-only: Windows PowerShell 5.1 reads an unmarked .ps1 as ANSI, not UTF-8.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Forward slashes throughout: every consumer here (.NET, cargo) accepts them on Windows, and the script
# stays runnable under pwsh on Linux/macOS, where CI and local tests exercise this logic.
$root = if ($env:CLAUDE_PLUGIN_ROOT) { $env:CLAUDE_PLUGIN_ROOT } else { Split-Path -Parent $PSScriptRoot }
$bin = "$root/target/release/claude-kanban.exe"
$defaultBase = 'https://github.com/CjS77/claude-kanban/releases/download'
$baseUrl = if ($env:KANBAN_RELEASE_BASE_URL) { $env:KANBAN_RELEASE_BASE_URL } else { $defaultBase }

function Write-Log([string] $msg) { [Console]::Error.WriteLine("kanban-mcp: $msg") }

# The plugin manifest's "version" -- the launcher is pinned to it, never "latest": an updated plugin
# must never run a stale binary, and a not-yet-updated plugin must never fetch a newer one.
function Get-PluginVersion {
    try {
        $v = [string] (Get-Content -Raw -Path "$root/.claude-plugin/plugin.json" | ConvertFrom-Json).version
        if ($v -match '^[0-9]') { $v } else { $null }
    } catch { $null }
}

# What the installed binary reports ("claude-kanban 1.0.0" -> "1.0.0"); $null when it won't run.
function Get-BinaryVersion {
    $ErrorActionPreference = 'Continue' # a native command writing to stderr must not become a terminating error
    try {
        $reported = & $bin --version 2>$null
        if ($LASTEXITCODE -ne 0 -or -not $reported) { return $null }
        ("$reported".Trim() -split '\s+')[-1]
    } catch { $null }
}

# The release target triple for this machine, or $null when no prebuilt binary is published for it
# (windows arm64 has no published target and falls through to the cargo path).
function Get-ReleaseTarget {
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($arch -eq 'AMD64') { 'x86_64-pc-windows-msvc' } else { $null }
}

# First field of the .sha256 sibling (works for "hash" and "hash  filename" formats) against Get-FileHash.
# PowerShell's -eq on strings is case-insensitive, which absorbs Get-FileHash's uppercase hex.
function Test-Checksum([string] $archivePath, [string] $sha256Path) {
    try {
        $expected = [string] ((Get-Content -TotalCount 1 -Path $sha256Path) -split '\s+')[0]
        $actual = (Get-FileHash -Algorithm SHA256 -Path $archivePath).Hash
        ($expected -ne '') -and ($expected -eq $actual)
    } catch { $false }
}

# Fetch, verify, and install the release binary. Staging happens in a random-named dir inside target/release
# so the final Move-Item is an atomic same-filesystem rename: with parallel workers or two projects, two
# sessions can race the first launch, and a half-written binary must never be exec'd.
function Install-Release {
    $target = Get-ReleaseTarget
    if (-not $target) { Write-Log "no prebuilt binary for windows/$env:PROCESSOR_ARCHITECTURE"; return $false }
    $version = Get-PluginVersion
    if (-not $version) { Write-Log "cannot read `"version`" from $root/.claude-plugin/plugin.json"; return $false }
    $archive = "claude-kanban-$target.zip"
    $url = "$baseUrl/v$version/$archive"
    $staging = "$root/target/release/.fetch." + [IO.Path]::GetRandomFileName()
    try { New-Item -ItemType Directory -Force -Path $staging | Out-Null } catch { Write-Log "cannot create $staging"; return $false }
    Write-Log "fetching prebuilt v$version for $target..."
    try {
        # Old Windows PowerShell defaults exclude TLS 1.2; harmless everywhere else.
        try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor 3072 } catch {}
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile "$staging/$archive"
        Invoke-WebRequest -UseBasicParsing -Uri "$url.sha256" -OutFile "$staging/$archive.sha256"
    } catch {
        Remove-Item -Recurse -Force -Path $staging
        Write-Log "download failed ($url)"
        return $false
    }
    if (-not (Test-Checksum "$staging/$archive" "$staging/$archive.sha256")) {
        Remove-Item -Recurse -Force -Path $staging
        Write-Log "checksum mismatch for $archive -- refusing the download"
        return $false
    }
    try {
        Expand-Archive -Path "$staging/$archive" -DestinationPath $staging -Force
        if (-not (Test-Path "$staging/claude-kanban.exe")) { throw 'claude-kanban.exe missing from the archive' }
        Move-Item -Force -Path "$staging/claude-kanban.exe" -Destination $bin
    } catch {
        Remove-Item -Recurse -Force -Path $staging
        Write-Log "could not unpack claude-kanban.exe from $archive"
        return $false
    }
    Remove-Item -Recurse -Force -Path $staging
    $true
}

function Invoke-CargoBuild {
    $ErrorActionPreference = 'Continue' # cargo narrates on stderr; that must not become a terminating error
    $cargo = Get-Command -Name cargo -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $cargo) {
        Write-Log "cargo not found -- install Rust from https://rustup.rs and run 'cargo build --release' in $root, or download"
        Write-Log "the release asset for your platform from https://github.com/CjS77/claude-kanban/releases into $root/target/release/"
        exit 1
    }
    Write-Log 'building claude-kanban from source (a minute or two)...'
    & $cargo.Source build --release --manifest-path "$root/Cargo.toml" 2>&1 | ForEach-Object { [Console]::Error.WriteLine([string] $_) }
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

# Make $bin exist and match the plugin version: keep a current binary, refresh a stale or broken one
# (Cargo.toml and plugin.json move in lockstep, so --version is the arbiter), download when missing,
# and only then fall back to cargo. An unreadable plugin.json runs the existing binary rather than bricking.
function Confirm-Binary {
    if (Test-Path -Path $bin) {
        $want = Get-PluginVersion
        if (-not $want) { return }
        $have = Get-BinaryVersion
        if ($have -eq $want) { return }
        $haveText = if ($have) { $have } else { 'nothing' }
        Write-Log "installed binary reports '$haveText' but the plugin is v$want -- refreshing"
    }
    if (Install-Release) { return }
    Invoke-CargoBuild
}

Confirm-Binary
# Hand the session to the real binary with inherited stdio: -NoNewWindow passes this process's own raw
# stdin/stdout/stderr handles to the child, so the long-lived JSON-RPC session streams both ways with no
# PowerShell pipeline in between (the call operator would decode and re-emit stdout line by line instead).
$startArgs = @{ FilePath = $bin; NoNewWindow = $true; Wait = $true; PassThru = $true }
if ($args.Count -gt 0) {
    # Start-Process joins -ArgumentList with spaces verbatim, so quote anything that needs it.
    $startArgs.ArgumentList = @($args | ForEach-Object { if ("$_" -match '[\s"]') { '"' + ("$_" -replace '"', '\"') + '"' } else { "$_" } })
}
$child = Start-Process @startArgs
exit $child.ExitCode
