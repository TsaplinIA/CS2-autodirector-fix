$ErrorActionPreference = "Stop"

$repo = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$cargoToml = Get-Content -LiteralPath (Join-Path $repo "Cargo.toml")
$cargoVersionLine = $cargoToml | Where-Object { $_ -match '^version\s*=' } | Select-Object -First 1
if (-not $cargoVersionLine) {
    throw "Could not find package version in Cargo.toml"
}

$cargoVersion = ($cargoVersionLine -replace '^version\s*=\s*"', '') -replace '"\s*$', ''
if ($cargoVersion -notmatch '^(\d+)\.(\d+)\.(\d+)$') {
    throw "Cargo.toml version must use MAJOR.MINOR.PATCH format"
}

$major = [int]$Matches[1]
$minor = [int]$Matches[2]
$base = "$major.$minor"
$tagPattern = "v$base.*"

Push-Location $repo
try {
    $tags = git tag --list $tagPattern
} finally {
    Pop-Location
}

$maxPatch = -1
foreach ($tag in $tags) {
    if ($tag -match "^v$major\.$minor\.(\d+)$") {
        $patch = [int]$Matches[1]
        if ($patch -gt $maxPatch) {
            $maxPatch = $patch
        }
    }
}

$nextPatch = $maxPatch + 1
$version = "v$base.$nextPatch"

Write-Output $version
