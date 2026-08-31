function Assert-PackageProvenance {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [string]$BuildId
    )

    & git -C $Root diff --quiet --ignore-submodules --
    $worktreeDirty = $LASTEXITCODE -ne 0
    & git -C $Root diff --cached --quiet --ignore-submodules --
    $indexDirty = $LASTEXITCODE -ne 0
    if ($worktreeDirty -or $indexDirty) {
        throw "Packaging requires a clean tracked working tree."
    }

    $headOutput = & git -C $Root rev-parse HEAD
    $headExitCode = $LASTEXITCODE
    if ($headExitCode -ne 0 -or [string]::IsNullOrWhiteSpace([string]$headOutput)) {
        throw "Could not determine a committed Git build ID."
    }
    $head = ([string]$headOutput).Trim()
    if ($head -cnotmatch '^[0-9a-f]{40}$') {
        throw "Could not determine a committed Git build ID."
    }
    if ($BuildId -and ($BuildId -cnotmatch '^[0-9a-f]{40}$' -or $BuildId -cne $head)) {
        throw "BuildId must match the current committed HEAD."
    }
    return $head
}
