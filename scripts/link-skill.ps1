# link-skill.ps1 — link the canonical agy-guide skill into Claude Code, Antigravity,
# or a specific workspace without building or restarting agy-mcp.
#
# Creates a directory JUNCTION (no administrator privileges required) pointing at this
# repository's skills/agy-guide directory, so every session loads the live canonical copy
# rather than a snapshot that goes stale.
#
# Usage:
#   .\scripts\link-skill.ps1 -Claude                    # ~/.claude/skills/agy-guide
#   .\scripts\link-skill.ps1 -Antigravity               # ~/.gemini/config/skills/agy-guide (global)
#   .\scripts\link-skill.ps1 -Claude -Antigravity       # both global locations
#   .\scripts\link-skill.ps1 -Workspace C:\path\to\repo # <repo>\.agents\skills\agy-guide

param(
    [switch]$Claude,
    [Alias('Agy')]
    [switch]$Antigravity,
    [string]$Workspace
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$skillTarget = Join-Path $repoRoot 'skills\agy-guide'

if (-not (Test-Path $skillTarget)) {
    throw "Canonical skill directory not found at $skillTarget"
}

if (-not $Claude -and -not $Antigravity -and -not $Workspace) {
    Write-Host "Usage:"
    Write-Host "  .\scripts\link-skill.ps1 -Claude                    # link into ~/.claude/skills/agy-guide"
    Write-Host "  .\scripts\link-skill.ps1 -Antigravity               # link into ~/.gemini/config/skills/agy-guide"
    Write-Host "  .\scripts\link-skill.ps1 -Claude -Antigravity       # link into both global skill directories"
    Write-Host "  .\scripts\link-skill.ps1 -Workspace C:\path\to\repo # link into <repo>\.agents\skills\agy-guide"
    exit 0
}

function Set-SkillJunction([string]$LinkPath, [string]$TargetDir) {
    $existing = Get-Item -LiteralPath $LinkPath -Force -ErrorAction SilentlyContinue
    if ($existing -and -not $existing.LinkType) {
        Write-Warning "$LinkPath is a real directory, not a junction. Leaving it untouched."
        return
    }
    $want = [IO.Path]::GetFullPath($TargetDir)
    $have = if ($existing) { try { [IO.Path]::GetFullPath(@($existing.Target)[0]) } catch { $null } } else { $null }
    if ($have -and $have -ieq $want) {
        Write-Host "already linked: $LinkPath -> $want"
        return
    }
    if ($existing) { $existing.Delete() }
    $parent = Split-Path -Parent $LinkPath
    if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Force $parent | Out-Null }
    New-Item -ItemType Junction -Path $LinkPath -Target $want -ErrorAction Stop | Out-Null
    Write-Host "linked: $LinkPath -> $want"
}

if ($Claude) {
    $claudeLink = Join-Path $env:USERPROFILE '.claude\skills\agy-guide'
    Set-SkillJunction -LinkPath $claudeLink -TargetDir $skillTarget
}

if ($Antigravity) {
    $agyLink = Join-Path $env:USERPROFILE '.gemini\config\skills\agy-guide'
    Set-SkillJunction -LinkPath $agyLink -TargetDir $skillTarget
}

if ($Workspace) {
    if (-not (Test-Path $Workspace)) {
        throw "Workspace does not exist: $Workspace"
    }
    $wsRoot = [IO.Path]::GetFullPath($Workspace)
    $wsLink = Join-Path $wsRoot '.agents\skills\agy-guide'
    Set-SkillJunction -LinkPath $wsLink -TargetDir $skillTarget
    $gitExclude = Join-Path $wsRoot '.git\info\exclude'
    if (Test-Path $gitExclude) {
        $lines = Get-Content -LiteralPath $gitExclude -ErrorAction SilentlyContinue
        if ($lines -notcontains '.agents/skills/agy-guide') {
            Add-Content -LiteralPath $gitExclude -Value "`n.agents/skills/agy-guide"
            Write-Host "added .agents/skills/agy-guide to $gitExclude"
        }
    }
}
