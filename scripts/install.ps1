$ErrorActionPreference = "Stop"

$repo = "pedro-canedo/smith"
$bin = "smith"
$target = if ($env:SMITH_TARGET) { $env:SMITH_TARGET } else { "x86_64-pc-windows-msvc" }
$installDir = if ($env:SMITH_INSTALL_DIR) {
    $env:SMITH_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "smith\bin"
}

if ($target -ne "x86_64-pc-windows-msvc") {
    throw "unsupported Windows target '$target'; the published Windows binary is x86_64-pc-windows-msvc"
}

$version = $env:SMITH_VERSION
if (-not $version) {
    $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}
if (-not $version) {
    throw "could not resolve the latest smith release"
}
if ($version -notlike "v*") {
    $version = "v$version"
}

$plainVersion = $version -replace '^v', ''
$name = "$bin-$plainVersion-$target"
$archive = "$name.zip"
$base = "https://github.com/$repo/releases/download/$version"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("smith-install-" + [guid]::NewGuid())
$archivePath = Join-Path $tempDir $archive
$checksumPath = Join-Path $tempDir "$archive.sha256"
$extractDir = Join-Path $tempDir "extract"

try {
    New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
    Invoke-WebRequest "$base/$archive" -OutFile $archivePath
    Invoke-WebRequest "$base/$archive.sha256" -OutFile $checksumPath

    $expected = ((Get-Content $checksumPath -Raw) -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        throw "SHA-256 mismatch for $archive (expected $expected, got $actual)"
    }

    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir -Force
    $source = Join-Path $extractDir "$name\$bin.exe"
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "release archive does not contain $name\$bin.exe"
    }

    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
    Copy-Item -LiteralPath $source -Destination (Join-Path $installDir "$bin.exe") -Force

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $pathEntries = @($userPath -split ';' | Where-Object { $_ })
    if ($pathEntries -notcontains $installDir) {
        [Environment]::SetEnvironmentVariable("Path", (($pathEntries + $installDir) -join ';'), "User")
    }
    if (@($env:Path -split ';') -notcontains $installDir) {
        $env:Path = "$installDir;$env:Path"
    }

    Write-Host "installed $bin $version to $(Join-Path $installDir "$bin.exe")"
    Write-Host "restart PowerShell if another shell does not see the updated PATH"
}
finally {
    if (Test-Path -LiteralPath $tempDir) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force
    }
}
