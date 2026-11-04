# Installs a released bale binary on Windows.
#
#   irm https://raw.githubusercontent.com/bmcreations/bale/main/install.ps1 | iex
#
# BALE_VERSION picks a release (default: the latest), BALE_INSTALL_DIR the
# directory (default: %LOCALAPPDATA%\bale\bin). The directory is added to the
# user PATH unless BALE_NO_MODIFY_PATH is set. Then run `bale install` for the
# sweep and `bale install --hooks claude` (or another tool) for the hooks.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'bmcreations/bale'
$dir = if ($env:BALE_INSTALL_DIR) { $env:BALE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'bale\bin' }

# Releases ship an x86_64 build only; Windows on ARM runs it under emulation.
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne 'AMD64' -and $arch -ne 'ARM64') {
    throw "bale: no prebuilt binary for $arch; build from source with cargo"
}

$version = $env:BALE_VERSION
if (-not $version) {
    $latest = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -Headers @{ 'User-Agent' = 'bale-install' }
    $version = $latest.tag_name
}
$version = $version.TrimStart('v')

$archive = "bale-v$version-x86_64-pc-windows-msvc.zip"
$url = "https://github.com/$repo/releases/download/v$version/$archive"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("bale-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "bale: downloading $archive"
    Invoke-WebRequest -Uri $url -OutFile (Join-Path $tmp $archive) -UseBasicParsing
    Invoke-WebRequest -Uri "$url.sha256" -OutFile (Join-Path $tmp "$archive.sha256") -UseBasicParsing

    $expected = ((Get-Content (Join-Path $tmp "$archive.sha256") -Raw).Trim() -split '\s+')[0]
    $actual = (Get-FileHash (Join-Path $tmp $archive) -Algorithm SHA256).Hash
    if ($actual -ne $expected.ToUpperInvariant()) {
        throw "bale: checksum mismatch for $archive (expected $expected, got $actual)"
    }

    Expand-Archive -Path (Join-Path $tmp $archive) -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    $target = Join-Path $dir 'bale.exe'
    # Windows can't overwrite a running exe but can rename it, so move the
    # old one aside first; a running bale keeps its file.
    $old = Join-Path $dir '.bale.old.exe'
    Remove-Item $old -Force -ErrorAction SilentlyContinue
    if (Test-Path $target) { Move-Item $target $old -Force }
    Move-Item (Join-Path $tmp 'bale.exe') $target
    Remove-Item $old -Force -ErrorAction SilentlyContinue
    Write-Host "bale: installed $(& $target --version) to $target"
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$onPath = ($userPath -split ';') -contains $dir
if (-not $onPath) {
    if ($env:BALE_NO_MODIFY_PATH) {
        Write-Warning "bale: $dir is not on PATH; add it to your user PATH"
    } else {
        $newPath = if ($userPath) { "$userPath;$dir" } else { $dir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        $env:Path = "$env:Path;$dir"
        Write-Host "bale: added $dir to your user PATH; open a new terminal to pick it up"
    }
}
Write-Host "bale: next, run 'bale install' and 'bale install --hooks claude'"
