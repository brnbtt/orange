# Fast pre-push gate. Deliberately excludes `orange` and `orange-client`: they
# pull in GStreamer, GPUI and resvg, and a hook slow enough to be annoying is a
# hook that gets bypassed. Those crates are covered by the full suite that
# ship.ps1 runs before it commits anything.
#
# What is left still covers the relay, which is the one component where a bug
# breaks every user at once and which deploys on its own path.
#
# Install once:  git config core.hooksPath packaging/hooks
# Bypass once:   git push --no-verify

$ErrorActionPreference = "Stop"
$root = (& git rev-parse --show-toplevel).Trim()
Push-Location $root
try {
    $fast = @("-p", "orange-signal", "-p", "orange-relay", "-p", "orange-updater")

    Write-Host "pre-push: formatting" -ForegroundColor DarkGray
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt found unformatted code. Run: cargo fmt --all" }

    Write-Host "pre-push: clippy" -ForegroundColor DarkGray
    cargo clippy --locked @fast --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "clippy found problems" }

    Write-Host "pre-push: tests" -ForegroundColor DarkGray
    cargo test --locked @fast
    if ($LASTEXITCODE -ne 0) { throw "tests failed" }

    Write-Host "pre-push: ok" -ForegroundColor Green
}
finally { Pop-Location }
