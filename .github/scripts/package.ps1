param(
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$dist = Join-Path $repo "dist"
$payloadDir = Join-Path $dist "autodirector-fix"
$sourceDir = Join-Path $dist "source_code"
$releaseDir = Join-Path $repo "target\release"
$dll = Join-Path $releaseDir "autodirector_fix.dll"
$config = Join-Path $repo "autodirector-fix-config.toml"

function Resolve-PackageVersion {
    param([string]$Version)

    $cargoToml = Get-Content -LiteralPath (Join-Path $repo "Cargo.toml")
    $cargoVersionLine = $cargoToml | Where-Object { $_ -match '^version\s*=' } | Select-Object -First 1
    if (-not $cargoVersionLine) {
        throw "Could not find package version in Cargo.toml"
    }

    $cargoVersion = ($cargoVersionLine -replace '^version\s*=\s*"', '') -replace '"\s*$', ''
    if ($cargoVersion -notmatch '^(\d+)\.(\d+)\.(\d+)$') {
        throw "Cargo.toml version must use MAJOR.MINOR.PATCH format"
    }
    $cargoMajor = $Matches[1]
    $cargoMinor = $Matches[2]

    if ($Version -eq "") {
        return "v$cargoVersion"
    }

    if ($Version -notmatch '^v(\d+)\.(\d+)\.(\d+)$') {
        throw "Release version must use vMAJOR.MINOR.PATCH format"
    }

    if (($Matches[1] -ne $cargoMajor) -or ($Matches[2] -ne $cargoMinor)) {
        throw "Release version $Version does not match Cargo.toml release line $cargoMajor.$cargoMinor"
    }

    return $Version
}

function Copy-SourceTree {
    param([string]$Destination)

    $excludedDirs = @(
        ".git",
        ".github",
        ".idea",
        ".vs",
        ".vscode",
        "scripts",
        "target",
        "dist"
    )
    $excludedFiles = @(
        "*.zip",
        ".gitignore",
        "AUTODIRECTOR_REVERSE_SUMMARY_RU.md"
    )

    Get-ChildItem -LiteralPath $repo -Force | ForEach-Object {
        if ($_.PSIsContainer -and ($excludedDirs -contains $_.Name)) {
            return
        }
        if (-not $_.PSIsContainer) {
            foreach ($excludedFile in $excludedFiles) {
                if ($_.Name -like $excludedFile) {
                    return
                }
            }
        }
        if (-not $_.PSIsContainer -and ($excludedFiles -contains $_.Name)) {
            return
        }

        Copy-Item -LiteralPath $_.FullName -Destination $Destination -Recurse -Force
    }
}

Push-Location $repo
try {
    $resolvedVersion = Resolve-PackageVersion -Version $Version
    $payloadZip = Join-Path $repo "autodirector-fix-$resolvedVersion.zip"
    $sourceZip = Join-Path $repo "source_code-$resolvedVersion.zip"

    cargo build --release

    if (-not (Test-Path $dll)) {
        throw "Missing built DLL: $dll"
    }

    if (Test-Path $dist) {
        Remove-Item -LiteralPath $dist -Recurse -Force
    }
    New-Item -ItemType Directory -Path $payloadDir | Out-Null
    New-Item -ItemType Directory -Path $sourceDir | Out-Null

    Copy-Item -LiteralPath $dll -Destination (Join-Path $payloadDir "autodirector-fix-$resolvedVersion.dll")
    if (Test-Path $config) {
        Copy-Item -LiteralPath $config -Destination (Join-Path $payloadDir "autodirector-fix-config.toml")
    }

    Get-ChildItem -LiteralPath $repo -File -Filter "autodirector-fix*.zip" | Remove-Item -Force
    Get-ChildItem -LiteralPath $repo -File -Filter "source_code*.zip" | Remove-Item -Force

    Copy-SourceTree -Destination $sourceDir

    Compress-Archive -Path (Join-Path $payloadDir "*") -DestinationPath $payloadZip
    Compress-Archive -Path (Join-Path $sourceDir "*") -DestinationPath $sourceZip

    Write-Host "Package version: $resolvedVersion"
    Write-Host "Package created: $payloadZip"
    Write-Host "Package created: $sourceZip"
} finally {
    Pop-Location
}
