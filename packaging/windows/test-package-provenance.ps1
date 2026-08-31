$ErrorActionPreference = "Stop"

$helper = Join-Path $PSScriptRoot "package-provenance.ps1"
if (-not (Test-Path -LiteralPath $helper -PathType Leaf)) {
    throw "Expected RED: package provenance helper is absent"
}
. $helper

$root = Join-Path $env:TEMP ("orange-package-provenance-test-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $root | Out-Null
try {
    & git -C $root init --quiet
    & git -C $root config user.email "test@orange.invalid"
    & git -C $root config user.name "Orange Test"
    Set-Content -LiteralPath (Join-Path $root "tracked.txt") -Value "one" -Encoding ASCII
    & git -C $root add tracked.txt
    & git -C $root commit --quiet -m initial
    $build = (& git -C $root rev-parse HEAD).Trim()

    if ((Assert-PackageProvenance -Root $root -BuildId $build) -cne $build) {
        throw "clean repository did not return its build ID"
    }
    if ((Assert-PackageProvenance -Root $root) -cne $build) {
        throw "clean developer packaging did not infer its build ID"
    }

    Set-Content -LiteralPath (Join-Path $root "diagnostics.log") -Value "untracked" -Encoding ASCII
    Assert-PackageProvenance -Root $root -BuildId $build | Out-Null

    Set-Content -LiteralPath (Join-Path $root "tracked.txt") -Value "dirty" -Encoding ASCII
    try {
        Assert-PackageProvenance -Root $root -BuildId $build | Out-Null
        throw "tracked worktree change was accepted"
    } catch {
        if ($_.Exception.Message -eq "tracked worktree change was accepted") { throw }
    }

    & git -C $root add tracked.txt
    try {
        Assert-PackageProvenance -Root $root -BuildId $build | Out-Null
        throw "staged change was accepted"
    } catch {
        if ($_.Exception.Message -eq "staged change was accepted") { throw }
    }

    & git -C $root commit --quiet -m changed
    try {
        Assert-PackageProvenance -Root $root -BuildId $build | Out-Null
        throw "changed HEAD was accepted"
    } catch {
        if ($_.Exception.Message -eq "changed HEAD was accepted") { throw }
    }

    Write-Host "RESULT: package provenance checks passed"
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
