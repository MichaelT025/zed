# Builds the fork's Windows installer from `integration` and publishes it as a
# `mine-YYYY.MM.DD` GitHub release, which installed fork builds pick up through
# `crates/fork_updater`.
#
# Usage: pwsh script/fork-release.ps1 [-DryRun] [-SkipBuild] [-Tag <tag>]
#
# Exit codes: 0 published (or dry run succeeded), 1 failed. On success the last
# line of output is `RELEASE_URL=<url>` (or `DRY_RUN_INSTALLER=<path>`), so a
# caller can parse it.

[CmdletBinding()]
Param(
    # Build and stage everything, but don't create the GitHub release.
    [Parameter()][switch]$DryRun,
    # Reuse the installer from the previous build instead of rebuilding.
    [Parameter()][switch]$SkipBuild,
    # Override the release tag (defaults to mine-YYYY.MM.DD, with -2, -3... on repeats).
    [Parameter()][string]$Tag,
    [Parameter()][string]$Repository = "MichaelT025/zed",
    [Parameter()][string]$Branch = "integration"
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

# Must match INSTALLER_ASSET_NAME in crates/fork_updater/src/fork_updater.rs.
$InstallerAssetName = "ZedFork-Setup-x86_64.exe"
$Architecture = "x86_64"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$BundleScript = Join-Path $PSScriptRoot "bundle-windows.ps1"
$PatchedBundleScript = Join-Path $PSScriptRoot ".fork-bundle-windows.tmp.ps1"
$UpstreamIsccLine = '$innoSetupPath = "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"'
$VsCmakeDir = "C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"

function Write-Step([string]$Message) {
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Fail([string]$Message) {
    Write-Host "ERROR: $Message" -ForegroundColor Red
    exit 1
}

function Find-Iscc {
    $candidates = @(
        "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
    )
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }
    return $null
}

# bundle-windows.ps1 leaves these in the repo root.
function Remove-BundleLeftovers {
    $patterns = @("AGS_SDK*", "conpty*", "Microsoft.Windows.Console.ConPTY*.nupkg", "inno")
    foreach ($pattern in $patterns) {
        Get-ChildItem -LiteralPath $RepoRoot -Filter $pattern -Force -ErrorAction SilentlyContinue | ForEach-Object {
            $tracked = git -C $RepoRoot ls-files -- $_.Name
            if (-not $tracked) {
                Remove-Item -LiteralPath $_.FullName -Recurse -Force
            }
        }
    }
    if (Test-Path -LiteralPath $PatchedBundleScript) {
        Remove-Item -LiteralPath $PatchedBundleScript -Force
    }
}

function Get-NextTag {
    $base = "mine-" + (Get-Date -Format "yyyy.MM.dd")
    $existing = @(git -C $RepoRoot ls-remote --tags origin "refs/tags/$base*" | ForEach-Object { ($_ -split "refs/tags/")[1] })
    if ($existing -notcontains $base) {
        return $base
    }
    $suffix = 2
    while ($existing -contains "$base-$suffix") {
        $suffix += 1
    }
    return "$base-$suffix"
}

Set-Location -LiteralPath $RepoRoot

Write-Step "Preflight"
$currentBranch = (git branch --show-current).Trim()
if ($currentBranch -ne $Branch) {
    Fail "must run on '$Branch' (currently on '$currentBranch')"
}
Remove-BundleLeftovers
$dirty = git status --porcelain
if ($dirty) {
    Fail "working tree is not clean:`n$($dirty -join "`n")"
}
git fetch --quiet --tags origin $Branch
$headSha = (git rev-parse HEAD).Trim()
$remoteSha = (git rev-parse "origin/$Branch").Trim()
if ($headSha -ne $remoteSha) {
    Fail "HEAD ($headSha) is not pushed: origin/$Branch is $remoteSha. Push first so the release commit exists on GitHub."
}
$channel = (Get-Content -LiteralPath (Join-Path $RepoRoot "crates\zed\RELEASE_CHANNEL") -Raw).Trim()
if ($channel -ne "dev") {
    Fail "crates/zed/RELEASE_CHANNEL is '$channel'; fork builds must stay on 'dev' so they install as Zed Dev"
}
try {
    gh auth status --hostname github.com *> $null
}
catch {
    Fail "gh is not authenticated: $_"
}
$iscc = Find-Iscc
if (-not $iscc) {
    Fail "Inno Setup 6 (ISCC.exe) not found"
}
if (-not (Get-Command cargo-about -ErrorAction SilentlyContinue)) {
    Fail "cargo-about is not installed"
}

if (-not $Tag) {
    $Tag = Get-NextTag
}
$version = (cargo metadata --no-deps --offline --format-version 1 | ConvertFrom-Json).packages |
    Where-Object { $_.name -eq "zed" } |
    Select-Object -ExpandProperty version
$previousTag = try {
    (gh release view --repo $Repository --json tagName --jq .tagName 2>$null).Trim()
}
catch {
    $null
}
Write-Host "  commit:   $headSha"
Write-Host "  version:  $version"
Write-Host "  tag:      $Tag"
Write-Host "  previous: $previousTag"

$installerPath = Join-Path $RepoRoot "target\Zed-$Architecture.exe"

if (-not $SkipBuild) {
    Write-Step "Building installer (this takes a while)"
    $bundleSource = Get-Content -LiteralPath $BundleScript -Raw
    if (-not $bundleSource.Contains($UpstreamIsccLine)) {
        Fail "bundle-windows.ps1 no longer contains the expected ISCC path line; update fork-release.ps1"
    }
    $patchedSource = $bundleSource.Replace($UpstreamIsccLine, "`$innoSetupPath = `"$iscc`"")
    Set-Content -LiteralPath $PatchedBundleScript -Value $patchedSource -NoNewline

    $buildStarted = Get-Date
    $env:ZED_COMMIT_SHA = $headSha
    if (Test-Path -LiteralPath $VsCmakeDir) {
        $env:PATH = "$VsCmakeDir;$env:PATH"
    }
    try {
        & pwsh -NoProfile -File $PatchedBundleScript -Architecture $Architecture
        if ($LASTEXITCODE -ne 0) {
            Fail "bundle-windows.ps1 failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Remove-BundleLeftovers
    }

    if (-not (Test-Path -LiteralPath $installerPath) -or (Get-Item -LiteralPath $installerPath).LastWriteTime -lt $buildStarted) {
        Fail "expected a fresh installer at $installerPath"
    }
}
elseif (-not (Test-Path -LiteralPath $installerPath)) {
    Fail "-SkipBuild given but no installer at $installerPath"
}

$stagingDir = Join-Path $RepoRoot "target\fork-release"
New-Item -ItemType Directory -Force -Path $stagingDir | Out-Null
$assetPath = Join-Path $stagingDir $InstallerAssetName
Copy-Item -LiteralPath $installerPath -Destination $assetPath -Force

Write-Step "Writing release notes"
$mainSha = (git rev-parse main).Trim()
$forkCommitRange = if ($previousTag) { "$previousTag..HEAD" } else { "HEAD" }
$forkCommits = @(git log --no-merges --format="- %s (%h)" $forkCommitRange "^main")
$notes = @(
    "Built from ``$Branch`` @ $headSha (Zed v$version, upstream ``main`` @ $($mainSha.Substring(0, 10)))."
    ""
    if ($previousTag) { "Fork changes since ${previousTag}:" } else { "Fork changes:" }
    ""
    if ($forkCommits.Count -gt 0) { $forkCommits } else { "- None (upstream update only)" }
) -join "`n"
$notesPath = Join-Path $stagingDir "release-notes.md"
Set-Content -LiteralPath $notesPath -Value $notes
Write-Host $notes

if ($DryRun) {
    Write-Step "Dry run: not publishing"
    Write-Output "DRY_RUN_INSTALLER=$assetPath"
    exit 0
}

Write-Step "Publishing $Tag"
$title = "Zed fork $($Tag -replace '^mine-', '') (v$version)"
gh release create $Tag $assetPath --repo $Repository --target $headSha --title $title --notes-file $notesPath --latest
if ($LASTEXITCODE -ne 0) {
    Fail "gh release create failed"
}

# The updater compares this field with the running build's commit SHA.
$published = gh release view $Tag --repo $Repository --json targetCommitish,url,assets | ConvertFrom-Json
if ($published.targetCommitish -ne $headSha) {
    Fail "release targetCommitish is '$($published.targetCommitish)', expected $headSha"
}
if (-not ($published.assets | Where-Object { $_.name -eq $InstallerAssetName })) {
    Fail "release has no $InstallerAssetName asset"
}

Write-Output "RELEASE_URL=$($published.url)"
