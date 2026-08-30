$ErrorActionPreference = "Stop"
$iss = Get-Content -LiteralPath (Join-Path $PSScriptRoot "orange.iss") -Raw
$package = Get-Content -LiteralPath (Join-Path (Split-Path $PSScriptRoot -Parent | Split-Path -Parent) "package.ps1") -Raw

$checks = [ordered]@{
    "directory page disabled" = 'DisableDirPage=yes'
    "program group page disabled" = 'DisableProgramGroupPage=yes'
    "ready page disabled" = 'DisableReadyPage=yes'
    "finished page disabled" = 'DisableFinishedPage=yes'
    "force close support" = 'CloseApplications=force'
    "updater installed" = 'Source: "\.\.\\\.\.\\target\\release\\orange-updater\.exe"'
    "interactive launch" = 'Filename: "\{app\}\\orange-tray\.exe";[^\r\n]*nowait[^\r\n]*skipifsilent'
    "no task page" = '(?m)^\[Tasks\]\s*$'
    "updater release build" = 'cargo build --locked --release -p orange -p orange-tray -p orange-updater'
    "beta build channel" = 'ORANGE_UPDATE_CHANNEL\s*=\s*"beta"'
    "automatic install click" = 'CurPageID\s*=\s*wpReady[\s\S]*PostMessage\(WizardForm\.NextButton\.Handle,\s*CN_COMMAND'
}

$failures = New-Object System.Collections.Generic.List[string]
foreach ($check in $checks.GetEnumerator()) {
    $matched = if ($check.Key -eq "no task page") {
        $iss -notmatch $check.Value
    } elseif ($check.Key -in @("updater release build", "beta build channel")) {
        $package -match $check.Value
    } else {
        $iss -match $check.Value
    }
    if (-not $matched) { $failures.Add($check.Key) }
}
if ($iss -match 'Tasks:\s*(desktopicon|startup)') {
    $failures.Add("optional shortcut task remains")
}
if ($failures.Count) {
    throw "Installer checks failed: $($failures -join ', ')"
}
Write-Host "RESULT: installer checks passed"
