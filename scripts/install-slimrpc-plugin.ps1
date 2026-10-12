# Copyright AGNTCY Contributors (https://github.com/agntcy)
# Copyright A2A Contributors (https://github.com/a2aproject)
# SPDX-License-Identifier: Apache-2.0
#
# Install a2a-transport-slimrpc, the SLIMRPC transport plugin for the Go a2a CLI
# (Windows only; macOS users: brew install a2a-transport-slimrpc).
#
# Usage:
#   irm https://raw.githubusercontent.com/a2aproject/a2a-rs/main/scripts/install-slimrpc-plugin.ps1 | iex
#
# Environment overrides:
#   A2A_SLIMRPC_PLUGIN_VERSION                    Install a specific version such as
#                                                 0.1.2, v0.1.2, or a2acli-transport-slimrpc-v0.1.2.
#   A2A_SLIMRPC_PLUGIN_INSTALL_DIR                Directory for the executable (default:
#                                                 %LOCALAPPDATA%\Programs\a2a-transport-slimrpc).
#   A2A_SLIMRPC_PLUGIN_CARGO_MANIFEST_URL         Override the Cargo.toml URL used to resolve the latest version.
#   A2A_SLIMRPC_PLUGIN_RELEASE_DOWNLOAD_BASE_URL  Override the release asset download base URL.

# Everything lives in a function so that `| iex` leaves no preferences or
# variables behind in the caller's session, and `throw` replaces `exit`, which
# would close the caller's terminal.
function Install-SlimrpcPlugin {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    Set-StrictMode -Version Latest

    $repo = 'a2aproject/a2a-rs'
    $prefix = 'a2acli-transport-slimrpc-v'
    $binary = 'a2a-transport-slimrpc'
    $target = 'x86_64-pc-windows-msvc'

    # Windows PowerShell 5.1 has no $IsWindows and is always Windows.
    $onWindows = ($PSVersionTable.PSEdition -ne 'Core') -or $IsWindows
    if (-not $onWindows) {
        throw 'this installer only supports Windows; macOS users: brew install a2a-transport-slimrpc'
    }
    if (-not [Environment]::Is64BitOperatingSystem) {
        throw 'a 64-bit version of Windows is required'
    }

    # Windows PowerShell 5.1 does not negotiate TLS 1.2 by default.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    # Names the URL on failure; the bare status message does not. Text comes
    # back as bytes for a non-text content type, and a byte array returned from
    # a function is unrolled, so decode it here.
    function Get-Url([string]$Url, [string]$OutFile) {
        try {
            if ($OutFile) { Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $OutFile; return }
            $content = (Invoke-WebRequest -UseBasicParsing -Uri $Url).Content
            if ($content -is [byte[]]) { [Text.Encoding]::UTF8.GetString($content) } else { $content }
        } catch {
            throw "failed to download ${Url}: $($_.Exception.Message)"
        }
    }

    $requested = $env:A2A_SLIMRPC_PLUGIN_VERSION
    if ($requested) {
        $version = $requested -replace ('^' + [regex]::Escape($prefix)), '' -replace '^v', ''
    } else {
        $manifestUrl = $env:A2A_SLIMRPC_PLUGIN_CARGO_MANIFEST_URL
        if (-not $manifestUrl) {
            $manifestUrl = "https://raw.githubusercontent.com/$repo/main/a2acli-transport-slimrpc/Cargo.toml"
        }
        $manifest = Get-Url $manifestUrl
        if ($manifest -notmatch '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"') {
            throw "failed to resolve the latest plugin version from $manifestUrl"
        }
        $version = $Matches[1]
    }
    if ($version -notmatch '^\d+\.\d+\.\d+([-+][0-9A-Za-z.-]+)?$') {
        throw "invalid plugin version '$version'"
    }

    $tag = "$prefix$version"
    $stem = "$binary-v$version-$target"
    $archiveName = "$stem.zip"
    $baseUrl = $env:A2A_SLIMRPC_PLUGIN_RELEASE_DOWNLOAD_BASE_URL
    if ($baseUrl) { $baseUrl = $baseUrl.TrimEnd('/') } else { $baseUrl = "https://github.com/$repo/releases/download" }

    $installDir = $env:A2A_SLIMRPC_PLUGIN_INSTALL_DIR
    if (-not $installDir) {
        $installDir = Join-Path (Join-Path $env:LOCALAPPDATA 'Programs') $binary
    }

    $work = Join-Path ([IO.Path]::GetTempPath()) ('a2a-slimrpc-' + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        $archive = Join-Path $work $archiveName
        $checksumFile = "$archive.sha256"

        Write-Host "==> downloading $archiveName"
        Get-Url "$baseUrl/$tag/$archiveName" $archive
        Get-Url "$baseUrl/$tag/$archiveName.sha256" $checksumFile

        $expected = ((Get-Content -Raw -Path $checksumFile).Trim() -split '\s+')[0].ToLowerInvariant()
        if (-not $expected) { throw "checksum file $archiveName.sha256 did not contain a SHA-256 value" }
        $actual = (Get-FileHash -Algorithm SHA256 -Path $archive).Hash.ToLowerInvariant()
        if ($expected -ne $actual) { throw 'downloaded archive checksum mismatch' }
        Write-Host "==> verified $archiveName"

        $extractDir = Join-Path $work 'extract'
        Expand-Archive -Path $archive -DestinationPath $extractDir -Force
        $source = Join-Path (Join-Path $extractDir $stem) "$binary.exe"
        if (-not (Test-Path -LiteralPath $source)) {
            throw "release archive did not contain $stem\$binary.exe"
        }

        New-Item -ItemType Directory -Force -Path $installDir | Out-Null
        $destination = Join-Path $installDir "$binary.exe"
        try {
            Copy-Item -LiteralPath $source -Destination $destination -Force
        } catch {
            throw "could not write $destination (is the plugin running?): $($_.Exception.Message)"
        }
        Write-Host "==> installed $binary $version to $destination"

        # The Go CLI discovers plugins on PATH, so put the directory there for
        # this user (no administrator rights needed) and for this session.
        $normalized = $installDir.TrimEnd('\')
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $entries = @()
        if ($userPath) { $entries = @($userPath -split ';' | Where-Object { $_ }) }
        if (-not ($entries | Where-Object { $_.TrimEnd('\') -ieq $normalized })) {
            [Environment]::SetEnvironmentVariable('Path', (($entries + $installDir) -join ';'), 'User')
            Write-Host "==> added $installDir to your user PATH; open a new terminal to pick it up there"
        }
        if (-not (($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -ieq $normalized })) {
            $env:Path = "$env:Path;$installDir"
        }

        try {
            Write-Host "==> $(& $destination --version)"
        } catch {
            Write-Warning "installed, but could not run $destination --version"
        }
        Write-Host 'Next: run `a2a transport list`; it should list `slimrpc`.'
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

try {
    Install-SlimrpcPlugin
} finally {
    Remove-Item -Path Function:\Install-SlimrpcPlugin -ErrorAction SilentlyContinue
}
