@echo off
rem MCP entry point on Windows. .mcp.json points every platform at the extensionless bin/kanban-mcp; Claude
rem Code's stdio spawn resolves commands with PATHEXT (vendored cross-spawn, then a cmd.exe /d /s /c wrap,
rem both PATHEXT-aware), so on Windows that path lands here -- no per-platform manifest entry exists or is
rem needed, and unix never sees this file. All real work happens in kanban-mcp.ps1 next door: this trampoline
rem only picks a PowerShell and passes the stdio handles straight through -- stdout belongs to JSON-RPC.
setlocal
set "ps=powershell.exe"
where /q powershell.exe || set "ps=pwsh.exe"
"%ps%" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0kanban-mcp.ps1" %*
exit /b %ERRORLEVEL%
