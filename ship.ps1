# Ships a new release: bump, test, commit, push, publish.
#
#   .\ship.ps1 -Notes "Fixes audio dropping out when the game loses focus."
#   .\ship.ps1 -Minor -Notes "Adds a settings screen."
#
# Versions are plain MAJOR.MINOR.PATCH. The leading 0 already says "not
# production ready" in semver, so there is no -beta suffix duplicating it; the
# update manifest carries the channel instead. 1.0.0 is where this arrives, not
# a change of scheme.
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
    # Bumps the patch number by default, because most releases are fixes.
    [switch]$Minor,
    [switch]$Major,
    # Sets the version outright, for anything the switches cannot express.
    [string]$Version,
    [switch]$SkipTests
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Push-Location $root
try {
    # Capture before dot-sourcing: publish-beta.ps1's param() block executes in
    # this scope and would otherwise overwrite $Notes and $Version with its own
    # defaults.
    $shipNotes = $Notes
    $shipVersion = $Version
    $shipMinor = [bool]$Minor
    $shipMajor = [bool]$Major
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
    if ($shipMinor -and $shipMajor) { throw "Pass -Minor or -Major, not both." }
    if ($shipVersion -and ($shipMinor -or $shipMajor)) {
        throw "Pass -Version or a bump switch, not both."
    }
    if (-not $shipVersion) {
        if ($current -cnotmatch '^(?<major>[0-9]+)\.(?<minor>[0-9]+)\.(?<patch>[0-9]+)$') {
            # Anything with a pre-release or build suffix is deliberately not
            # auto-bumped: what "next" means is a judgement call.
            throw "Cannot auto-bump '$current'. Pass an explicit -Version."
        }
        $shipVersion = if ($shipMajor) {
            "{0}.0.0" -f ([int]$Matches.major + 1)
        } elseif ($shipMinor) {
            "{0}.{1}.0" -f $Matches.major, ([int]$Matches.minor + 1)
        } else {
            "{0}.{1}.{2}" -f $Matches.major, $Matches.minor, ([int]$Matches.patch + 1)
        }
    }

    Invoke-Git fetch origin main
    $build = (& git rev-parse HEAD).Trim()
    Assert-Publishable -Version $shipVersion -Build $build -Notes $shipNotes

    Step "Shipping $current -> $shipVersion"
    if (-not $PSCmdlet.ShouldProcess("$shipVersion", "bump, commit, push and publish")) {
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
    $manifest = [IO.File]::ReadAllText($manifestPath)
    # A `$` anchor will not do: .NET matches it before `\n`, and Cargo.toml has
    # CRLF endings, so the `\r` sits between the quote and the anchor. The
    # lookahead matches the line end without consuming the `\r`, preserving it.
    $pattern = "(?m)^version = ""$([regex]::Escape($current))""(?=\r?$)"
    $bumped = $manifest -creplace $pattern, "version = ""$shipVersion"""
    if ($bumped -ceq $manifest) { throw "Could not find version = ""$current"" in Cargo.toml" }
    [IO.File]::WriteAllText($manifestPath, $bumped, (New-Object Text.UTF8Encoding($false)))

    # Every build is --locked, so a stale lock is a hard build failure.
    cargo update --workspace --quiet
    if ($LASTEXITCODE -ne 0) { throw "Could not update Cargo.lock" }

    Invoke-Git add Cargo.toml Cargo.lock
    Invoke-Git commit -m "Bump version to $shipVersion"
    Step "Pushing to origin/main"
    Invoke-Git push origin main

    # --- publish --------------------------------------------------------------
    & (Join-Path $root "publish-beta.ps1") -Publish -SkipTests -Notes $shipNotes
    if ($LASTEXITCODE -ne 0) { throw "Publish failed" }

    Write-Host ""
    Write-Host "Shipped $shipVersion." -ForegroundColor Green
    Write-Host "Clients pick it up at next launch, or within 6 hours if already running." -ForegroundColor DarkGray
}
finally { Pop-Location }
