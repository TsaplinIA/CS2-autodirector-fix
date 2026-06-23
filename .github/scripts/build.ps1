param(
    [ValidateSet("debug", "release")]
    [string]$Profile = "release"
)

$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$config = Join-Path $repo "autodirector-fix-config.toml"
Push-Location $repo
try {
    if ($Profile -eq "release") {
        cargo build --release
        $profileDir = Join-Path $repo "target\release"
    } else {
        cargo build
        $profileDir = Join-Path $repo "target\debug"
    }

    if (Test-Path $config) {
        Copy-Item -LiteralPath $config -Destination (Join-Path $profileDir "autodirector-fix-config.toml")
    }
} finally {
    Pop-Location
}
