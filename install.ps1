# install.ps1 — build agy-mcp and install it into Cargo's bin dir (~/.cargo/bin,
# already on PATH), then register it with Claude Code. This lands in the SAME place as the
# public `cargo install --git <url>` path, so dev and end-user installs are consistent.
#
# Why ~/.cargo/bin instead of registering target\release directly: a running MCP server
# holds an exclusive lock on the exe it was launched from. `cargo build` writes
# target\release, so it is NEVER blocked while the server runs.
#
# The deploy step does NOT stop the running servers. Stopping them used to race: during
# the ~2 min build, Claude Code sessions respawned agy-mcp and the copy then failed with
# "Access is denied" (2026-10-20). Windows forbids overwriting a running exe but allows
# RENAMING it, so we build first, rename the live exe aside to *.old, and drop the new one
# in place. Running servers keep using the renamed file; new launches pick up the new one.
#
# It also links the agy-guide skill into ~/.claude/skills so every workspace on the machine
# loads THIS repo's canonical copy (see AGENTS.md > Conventions).
#
# After the swap it smoke-tests the new exe over raw stdio (tools/list must return the 3
# tools) and lists the servers still running the previous binary, with the app that owns
# each. -Stop ends those, except any that owns a running job (killing it would turn the
# job LOST). Stopping AFTER the swap cannot race the copy: a respawn gets the new exe.
#
# Usage:  .\install.ps1 [-Stop]    then restart Claude Code (and any app listed).

param([switch]$Stop)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$cargoBin = if ($env:CARGO_HOME) { Join-Path $env:CARGO_HOME 'bin' } else { Join-Path $env:USERPROFILE '.cargo\bin' }
$exe = Join-Path $cargoBin 'agy-mcp.exe'

Write-Host "[1/5] cargo build --release   (running agy-mcp servers are left alone)"
Push-Location $root
cargo build --release
$code = $LASTEXITCODE
Pop-Location
if ($code -ne 0) { throw "cargo build failed (exit $code)" }
$built = Join-Path $root 'target\release\agy-mcp.exe'

Write-Host "[2/5] swapping $exe (rename the live one aside, then copy the new one in)"
$swapTime = Get-Date
if (-not (Test-Path $cargoBin)) { New-Item -ItemType Directory -Force $cargoBin | Out-Null }
# Sweep leftovers from earlier swaps; ones still held by a running server stay until next time.
Get-ChildItem -Path $cargoBin -Filter 'agy-mcp.exe.old*' -ErrorAction SilentlyContinue |
    ForEach-Object { try { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction Stop } catch {} }
if (Test-Path $exe) {
    $aside = "$exe.old"
    if (Test-Path $aside) { $aside = "$exe.old-$(Get-Date -Format yyyyMMddHHmmss)" }
    Rename-Item -LiteralPath $exe -NewName (Split-Path -Leaf $aside)
}
Copy-Item -LiteralPath $built -Destination $exe

Write-Host "[3/5] checking the Antigravity CLI (agy) is present, then registering if needed..."
# agy-mcp auto-detects agy at %LOCALAPPDATA%\agy\bin\agy.exe (else PATH), so we don't
# pass -e AGY_BIN here. Warn early if agy is missing — otherwise the failure only
# surfaces on the first tool call.
$agy = Join-Path $env:LOCALAPPDATA 'agy\bin\agy.exe'
if (-not (Test-Path $agy) -and -not (Get-Command agy -ErrorAction SilentlyContinue)) {
    Write-Warning "agy not found at $agy or on PATH. Install it: irm https://antigravity.google/cli/install.ps1 | iex  (then run 'agy' once and sign in). Registering anyway."
}

# Register by BARE NAME, not by absolute path. `cargo install` above rewrites
# ~/.cargo/bin/agy-mcp.exe in place and that dir is already on PATH, so a
# name-based registration NEVER has to be rewritten on a deploy — the only thing a
# deploy needs is a Claude Code restart. It also matches what Claude Desktop's own
# config and the public install instructions already use, so all three agree.
$desired = 'agy-mcp'
# Read the existing registration directly rather than through the CLI: this has to
# be able to say "nothing to do", and only the stored `command` answers that.
$claudeJson = Join-Path $env:USERPROFILE '.claude.json'
$current = $null
if (Test-Path $claudeJson) {
    try { $current = (Get-Content $claudeJson -Raw | ConvertFrom-Json).mcpServers.agy.command } catch {}
}
if ($current -eq $desired) {
    # Leave it ALONE. `claude mcp remove` + `add` would silently drop any `-e` env
    # (AGY_PERMISSIONS, AGY_WORKSPACE...) added to that entry by hand, and a deploy
    # must not quietly change what the operator decided at setup time.
    Write-Host "      already registered as '$desired' — untouched (any -e env on it is preserved)."
} else {
    if ($current) { Write-Host "      re-registering '$current' -> '$desired'; any -e env on the old entry is DROPPED." }
    claude mcp remove agy -s user            # no-op if not registered yet
    # Adding an env var means doing it yourself once — this script won't re-add it,
    # and won't remove it either:
    #   claude mcp remove agy -s user
    #   claude mcp add agy -s user -e AGY_PERMISSIONS=trusted -- agy-mcp
    # Non-standard agy path? `-e AGY_BIN="<full path to agy.exe>"` (auto-detected otherwise).
    # Want agy to default to a project dir (so an omitted `workspace` reuses its
    # target/ build cache instead of a cold scratch dir)? `-e AGY_WORKSPACE="<project root>"`.
    # AGY_PERMISSIONS=trusted removes the permission gate entirely (agy runs with
    # --dangerously-skip-permissions: no soft-deny recovery round-trips, no allowlist
    # to maintain). Deliberately a SETUP-time switch — it says this machine's
    # sub-agents may run anything, which is the operator's standing decision, not a
    # per-call arg a model could reach for. Default (unset) keeps the gate plus the
    # pre-granted build/test families.
    claude mcp add agy -s user -- $desired
}

# NOTE: a registration in Claude Desktop's own config
# (%APPDATA%\Claude\claude_desktop_config.json) is NOT a duplicate of this one and must not
# be removed. They serve different surfaces: this line registers the server for Claude Code,
# the Desktop config registers it for the Desktop app itself. Removing the Desktop entry on
# 2026-08-14 (mistaking it for a duplicate) made agy-mcp disappear from Desktop entirely.
# Two servers do run, and each does load the tool schema — that is the price of using it in
# both places, not a misconfiguration to fix.

Write-Host "[4/5] linking the agy-guide skill into ~/.claude/skills (global delivery)..."
# The canonical SKILL.md lives in this repo at skills/agy-guide/ and is delivered to EVERY
# workspace through this one link. It is a JUNCTION, not a symlink: a symlink needs admin
# (measured — New-Item -ItemType SymbolicLink fails with "Administrator privilege required"),
# a junction does not. Deliberately not a copy: a copy is what goes stale, and a stale copy
# under some repo's .claude/skills silently shadows this one (project scope beats user scope).
$skillTarget = Join-Path $root 'skills\agy-guide'
$skillLink = Join-Path $env:USERPROFILE '.claude\skills\agy-guide'
if (-not (Test-Path $skillTarget)) {
    Write-Warning "      $skillTarget missing - skipping the skill link."
} else {
    $existing = Get-Item -LiteralPath $skillLink -Force -ErrorAction SilentlyContinue
    if ($existing -and -not $existing.LinkType) {
        # A REAL directory sits there. Never delete it - it may hold someone's own skill.
        Write-Warning "      $skillLink is a real directory, not a link. Leaving it alone; agy-guide is served from there, not from this repo."
    } else {
        $want = [IO.Path]::GetFullPath($skillTarget)
        $have = if ($existing) { try { [IO.Path]::GetFullPath(@($existing.Target)[0]) } catch { $null } } else { $null }
        if ($have -and $have -ieq $want) {
            Write-Host "      already linked -> $want (untouched)."
        } else {
            if ($existing) { $existing.Delete() }   # removes the LINK only, never the target's contents
            $parent = Split-Path -Parent $skillLink
            if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Force $parent | Out-Null }
            try {
                New-Item -ItemType Junction -Path $skillLink -Target $want -ErrorAction Stop | Out-Null
                Write-Host "      linked $skillLink -> $want"
            } catch {
                Write-Warning "      could not create the junction: $($_.Exception.Message)"
            }
        }
    }
}

Write-Host "[5/5] smoke test, then the servers still on the previous binary..."
# Raw stdio like zenith-mcp's script: cmd `type` adds no BOM, and EOF ends the server.
$req = Join-Path ([IO.Path]::GetTempPath()) "agy-mcp-smoke-$PID.jsonl"
$lines = @(
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}',
    '{"jsonrpc":"2.0","method":"notifications/initialized"}',
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
)
[IO.File]::WriteAllText($req, ($lines -join "`n") + "`n", (New-Object Text.UTF8Encoding $false))
try {
    $out = cmd /d /s /c "type `"$req`" | `"$exe`" 2>nul" | Out-String
} finally {
    Remove-Item -LiteralPath $req -Force -ErrorAction SilentlyContinue
}
$tools = @(($out -split "`n" | Where-Object { $_.Trim() } | ForEach-Object { $_ | ConvertFrom-Json } |
    Where-Object { $_.id -eq 2 }).result.tools.name)
if ($tools.Count -ne 3) { throw "smoke: expected 3 tools from $exe, got: $($tools -join ', ')" }
Write-Host "      OK: $($tools -join ', ')"

# A server started before the swap still runs the old exe until its app restarts.
$stale = @(Get-CimInstance Win32_Process -Filter "Name='agy-mcp.exe'" |
    Where-Object { $_.CreationDate -lt $swapTime })
# Job records carry the PID of the server that owns them (~/.agy-mcp/jobs/<id>.json).
$busy = @(Get-ChildItem (Join-Path $env:USERPROFILE '.agy-mcp\jobs') -Filter 'j*.json' -ErrorAction SilentlyContinue |
    ForEach-Object { try { Get-Content $_.FullName -Raw | ConvertFrom-Json } catch {} } |
    Where-Object { $_.status -eq 'running' -and $_.owner_pid } | ForEach-Object { [int]$_.owner_pid })
foreach ($p in $stale) {
    $parent = (Get-CimInstance Win32_Process -Filter "ProcessId=$($p.ParentProcessId)" -ErrorAction SilentlyContinue).Name
    if (-not $parent) { $parent = 'parent gone' }
    $note = "pid $($p.ProcessId), started $($p.CreationDate.ToString('HH:mm')), under $parent"
    if (-not $Stop) { Write-Host "      old binary: $note" }
    elseif ($busy -contains [int]$p.ProcessId) { Write-Host "      kept (owns a running job): $note" }
    else {
        Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
        Write-Host "      stopped: $note"
    }
}
if (-not $stale) { Write-Host "      none." }
elseif (-not $Stop) { Write-Host "      restart those apps, or rerun with -Stop to end the idle ones." }

Write-Host ""
Write-Host "Done. RESTART Claude Code to pick up the new binary."
