$ErrorActionPreference = "Stop"
$iss = Get-Content -LiteralPath (Join-Path $PSScriptRoot "orange.iss") -Raw
$package = Get-Content -LiteralPath (Join-Path (Split-Path $PSScriptRoot -Parent | Split-Path -Parent) "package.ps1") -Raw
$provenance = Get-Content -LiteralPath (Join-Path $PSScriptRoot "package-provenance.ps1") -Raw

$checks = [ordered]@{
    "directory page disabled" = 'DisableDirPage=yes'
    "program group page disabled" = 'DisableProgramGroupPage=yes'
    "ready page disabled" = 'DisableReadyPage=yes'
    "finished page disabled" = 'DisableFinishedPage=yes'
    "force close support" = 'CloseApplications=force'
    "updater installed" = 'Source: "\.\.\\\.\.\\target\\release\\orange-updater\.exe"'
    "app-local VC runtime" = 'Source: "\.\.\\\.\.\\target\\package\\vcruntime140\.dll"; DestDir: "\{app\}"'
    "MIT license installed" = 'Source: "\.\.\\\.\.\\LICENSE"; DestDir: "\{app\}"'
    "interactive launch" = 'Filename: "\{app\}\\orange-tray\.exe";[^\r\n]*nowait[^\r\n]*skipifsilent'
    "shortcut shares taskbar identity" = 'Name: "\{group\}\\orange";[^\r\n]*AppUserModelID: "brnbtt\.orange"'
    "existing taskbar pin is migrated" = 'Name: "\{userappdata\}\\Microsoft\\Internet Explorer\\Quick Launch\\User Pinned\\TaskBar\\orange";[^\r\n]*AppUserModelID: "brnbtt\.orange";[^\r\n]*Check: IsOrangeTaskbarPin'
    "pin migration verifies target" = 'function IsOrangeTaskbarPin[\s\S]*CreateShortcut[\s\S]*TargetPath[\s\S]*\{app\}\\orange-tray\.exe'
    "no task page" = '(?m)^\[Tasks\]\s*$'
    "bundled media runtime" = 'Source: "\.\.\\\.\.\\target\\package\\gstreamer\\\*"; DestDir: "\{app\}\\gstreamer"[^\r\n]*recursesubdirs'
    "media runtime staged before Inno" = 'Copy-GStreamerRuntime[\s\S]*& \$iscc'
    "updater release build" = 'cargo build --locked --release -p orange -p orange-client -p orange-updater'
    "beta build channel" = 'ORANGE_UPDATE_CHANNEL\s*=\s*"beta"'
    "automatic install click" = 'CurPageID\s*=\s*wpReady[\s\S]*PostMessage\(WizardForm\.NextButton\.Handle,\s*CN_COMMAND'
}

$failures = New-Object System.Collections.Generic.List[string]
foreach ($check in $checks.GetEnumerator()) {
    $matched = if ($check.Key -eq "no task page") {
        $iss -notmatch $check.Value
    } elseif ($check.Key -in @("updater release build", "beta build channel", "media runtime staged before Inno")) {
        $package -match $check.Value
    } else {
        $iss -match $check.Value
    }
    if (-not $matched) { $failures.Add($check.Key) }
}
if ($iss -match 'Tasks:\s*(desktopicon|startup)') {
    $failures.Add("optional shortcut task remains")
}
if ($iss -match 'vc_redist|Microsoft Visual C\+\+ runtime installer') {
    $failures.Add("machine-wide VC runtime installer remains")
}
# Setup must not reach the network. A 504 MB download from a host with no CDN
# was the whole reason a first install could take minutes or fail; the runtime
# is carried in [Files] now, and nothing should quietly reintroduce a fetch.
if ($iss -match 'CreateDownloadPage|TDownloadWizardPage|DownloadTemporaryFile|https?://[^\r\n]*\.exe') {
    $failures.Add("installer downloads a prerequisite at setup time")
}
$provenanceCalls = [regex]::Matches($package, 'Assert-PackageProvenance -Root \$root -BuildId \$BuildId')
$tests = $package.IndexOf('if (-not $SkipTests)')
$build = $package.IndexOf('cargo build --locked --release')
if ($provenanceCalls.Count -ne 2 -or $provenanceCalls[0].Index -gt $tests -or $provenanceCalls[0].Index -gt $build) {
    $failures.Add("initial provenance check does not precede tests and build")
}
if ($package -cnotmatch 'Assert-PackageProvenance -Root \$root -BuildId \$BuildId \| Out-Null\r?\n\s*& \$iscc') {
    $failures.Add("final provenance check is not immediately before Inno")
}
$worktreeCheck = $provenance.IndexOf('git -C $Root diff --quiet')
$indexCheck = $provenance.IndexOf('git -C $Root diff --cached --quiet')
$headRead = $provenance.IndexOf('git -C $Root rev-parse HEAD')
$headExit = $provenance.IndexOf('$headExitCode = $LASTEXITCODE')
$headNonempty = $provenance.IndexOf('[string]::IsNullOrWhiteSpace([string]$headOutput)')
$headTrim = $provenance.IndexOf('$head = ([string]$headOutput).Trim()')
$buildValidation = $provenance.IndexOf('if ($BuildId -and')
$laterGit = if ($headRead -ge 0) { $provenance.IndexOf('git -C $Root', $headRead + 1) } else { -1 }
if ($worktreeCheck -lt 0 -or $indexCheck -le $worktreeCheck -or $headRead -le $indexCheck -or
    $headExit -le $headRead -or $headNonempty -le $headExit -or $headTrim -le $headNonempty -or
    $buildValidation -le $headTrim -or $laterGit -ge 0) {
    $failures.Add("provenance helper does not finish with a fresh HEAD validation")
}
if ($failures.Count) {
    throw "Installer checks failed: $($failures -join ', ')"
}
Write-Host "RESULT: installer checks passed"
