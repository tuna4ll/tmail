# Installs termail from the GitHub releases on Windows.
#
#   irm https://raw.githubusercontent.com/tuna4ll/termail/main/install.ps1 | iex
#
# $env:TERMAIL_VERSION      a release such as v1.0.0 (default: the latest)
# $env:TERMAIL_INSTALL_DIR  where the binary goes (default: %LOCALAPPDATA%\Programs\termail)

# A script block of its own, so nothing leaks into the session that ran irm | iex.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5.1 may still default to older TLS versions.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'tuna4ll/termail'
    $version = if ($env:TERMAIL_VERSION) { $env:TERMAIL_VERSION } else { 'latest' }
    $dir = if ($env:TERMAIL_INSTALL_DIR) { $env:TERMAIL_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\termail' }

    if (-not [Environment]::Is64BitOperatingSystem) {
        throw 'termail needs 64-bit Windows.'
    }
    # The x64 build also runs on Windows on ARM through emulation.
    $asset = 'termail-x86_64-pc-windows-msvc.zip'
    $base = if ($version -eq 'latest') {
        "https://github.com/$repo/releases/latest/download"
    } else {
        "https://github.com/$repo/releases/download/$version"
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("termail-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading $asset ($version)"
        $zip = Join-Path $tmp $asset
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $zip
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset.sha256" -OutFile "$zip.sha256"
        $expected = ((Get-Content "$zip.sha256" -Raw).Trim() -split '\s+')[0]
        $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash
        if ($actual -ne $expected) {
            throw 'The download does not match its checksum.'
        }
        Expand-Archive -Path $zip -DestinationPath $tmp -Force
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        Copy-Item (Join-Path $tmp 'termail.exe') (Join-Path $dir 'termail.exe') -Force
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($userPath -split ';') -contains $dir)) {
        $newPath = if ($userPath) { "$userPath;$dir" } else { $dir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        $env:Path = "$env:Path;$dir"
        Write-Host "Added $dir to your PATH. Open a new terminal to use it everywhere."
    }

    $installed = & (Join-Path $dir 'termail.exe') --version
    Write-Host "Installed $installed to $dir"
    Write-Host 'Run termail to start.'
}
