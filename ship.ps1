# Ships a new beta: bump, test, commit, push, publish.
#
#   .\ship.ps1 -Notes "Fixes audio dropping out when the game loses focus."
#
# Order matters. Everything that can fail cheaply runs before anything mutates
# the repo, so a failure never leaves you with a committed-and-pushed version
# bump to unwind.
#
# If the publish itself fails partway, do NOT re-run this script: it would bump
# the version again. Re-run the publisher directly instead, which is idempotent
# and converges on the same commit:
#
#   .\publish-beta.ps1 -Publish -Notes "<same notes>"

[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = "High")]
param(
    [Parameter(Mandatory = $true)][string]$Notes,
    # Defaults to bumping the trailing beta number. Pass this to start a new
    # series, e.g. -Version "0.3.0-beta.1".
    [string]$Version,
    [switch]$SkipTests
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Push-Location $root
try {
    . (Join-Path $root "publish-beta.ps1") -LibraryOnly

    function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
    function Invoke-Git {
        param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
        # git writes ordinary progress ("From https://...") to stderr. Under
        # ErrorActionPreference=Stop that becomes a terminating NativeCommandError,
        # so the exit code has to be the only success signal.
        $previous = $ErrorActionPreference
        try {
            $ErrorActionPreference = "Continue"
            & git @Arguments 2>&1 | ForEach-Object { Write-Host "   $_" -ForegroundColor DarkGray }
        } finally { $ErrorActionPreference = $previous }
        if ($LASTEXITCODE -ne 0) { throw "git $($Arguments -join ' ') failed" }
    }

    # --- checks that cost nothing --------------------------------------------
    Step "Checking the working tree"
    & git diff --quiet --ignore-submodules --
    $dirty = $LASTEXITCODE -ne 0
    & git diff --cached --quiet --ignore-submodules --
    $staged = $LASTEXITCODE -ne 0
    if ($dirty -or $staged) {
        throw "Shipping requires a clean tracked working tree. Commit or stash first."
    }

    $current = Get-WorkspaceVersion
    if (-not $Version) {
        if ($current -cnotmatch '^(?<series>[0-9]+\.[0-9]+\.[0-9]+-beta\.)(?<n>[0-9]+)$') {
            throw "Cannot auto-bump '$current'. Pass an explicit -Version."
        }
        $Version = "{0}{1}" -f $Matches.series, ([int]$Matches.n + 1)
    }

    Invoke-Git fetch origin main
    $build = (& git rev-parse HEAD).Trim()
    Assert-Publishable -Version $Version -Build $build -Notes $Notes

    Step "Shipping $current -> $Version"
    if (-not $PSCmdlet.ShouldProcess("$Version", "bump, commit, push and publish")) {
        return
    }

    # --- tests before any mutation -------------------------------------------
    # publish-beta.ps1 is invoked with -SkipTests below because this covers it.
    # Running here means a test failure costs nothing to recover from.
    if (-not $SkipTests) {
        Step "Running the workspace test suite"
        . (Join-Path $root "dev.ps1")
        cargo test --locked --workspace
        if ($LASTEXITCODE -ne 0) { throw "Workspace tests failed. Nothing was changed." }
    }

    # --- mutate ---------------------------------------------------------------
    Step "Bumping the workspace version"
    $manifestPath = Join-Path $root "Cargo.toml"
    $manifest = Get-Content -LiteralPath $manifestPath -Raw
    $bumped = $manifest -creplace "(?m)^version = ""$([regex]::Escape($current))""$", "version = ""$Version"""
    if ($bumped -ceq $manifest) { throw "Could not find version = ""$current"" in Cargo.toml" }
    [IO.File]::WriteAllText($manifestPath, $bumped, (New-Object Text.UTF8Encoding($false)))

    # Every build is --locked, so a stale lock is a hard build failure.
    cargo update --workspace --quiet
    if ($LASTEXITCODE -ne 0) { throw "Could not update Cargo.lock" }

    Invoke-Git add Cargo.toml Cargo.lock
    Invoke-Git commit -m "Bump version to $Version"
    Step "Pushing to origin/main"
    Invoke-Git push origin main

    # --- publish --------------------------------------------------------------
    & (Join-Path $root "publish-beta.ps1") -Publish -SkipTests -Notes $Notes
    if ($LASTEXITCODE -ne 0) { throw "Publish failed" }

    Write-Host ""
    Write-Host "Shipped $Version." -ForegroundColor Green
    Write-Host "Clients pick it up at next launch, or within 6 hours if already running." -ForegroundColor DarkGray
}
finally { Pop-Location }
