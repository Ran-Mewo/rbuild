# rbuild installer for Windows (PowerShell).
#
#   irm https://raw.githubusercontent.com/Ran-Mewo/rbuild/main/install.ps1 | iex
#
# Downloads the latest release zip for this architecture and installs rbuild.exe
# (and the static rbuildd daemon it pushes to the remote) into
# %LOCALAPPDATA%\Programs\rbuild, adding it to your user PATH.
#
# Override with env vars before piping to iex, e.g.:
#   $env:RBUILD_VERSION = 'v0.1.0'; irm .../install.ps1 | iex

$ErrorActionPreference = 'Stop'

$Repo       = if ($env:RBUILD_REPO)        { $env:RBUILD_REPO }        else { 'Ran-Mewo/rbuild' }
$Version    = if ($env:RBUILD_VERSION)     { $env:RBUILD_VERSION }     else { 'latest' }
$InstallDir = if ($env:RBUILD_INSTALL_DIR) { $env:RBUILD_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\rbuild' }

function Say($m) { Write-Host "rbuild-install: $m" }

# rbuild's Windows release is built for the MSVC target.
$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
    'AMD64' { $triple = 'x86_64-pc-windows-msvc' }
    'ARM64' { $triple = 'aarch64-pc-windows-msvc' }
    default { throw "unsupported architecture '$arch'." }
}

$archive = "rbuild-$triple.zip"
$base = if ($Version -eq 'latest') {
    "https://github.com/$Repo/releases/latest/download"
} else {
    "https://github.com/$Repo/releases/download/$Version"
}
$url = "$base/$archive"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("rbuild-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Say "downloading $url"
    $zip = Join-Path $tmp $archive
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing

    Say "installing to $InstallDir"
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    # Expand into the install dir; archive root holds rbuild.exe and daemon\.
    Expand-Archive -Path $zip -DestinationPath $InstallDir -Force

    if (-not (Test-Path (Join-Path $InstallDir 'rbuild.exe'))) {
        throw "archive did not contain rbuild.exe"
    }

    # Add to the user PATH (persisted) if not already present.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $InstallDir) {
        $newPath = if ([string]::IsNullOrEmpty($userPath)) { $InstallDir } else { "$userPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        # Make it usable in the current session too.
        $env:Path = "$env:Path;$InstallDir"
        Say "added $InstallDir to your user PATH (restart shells to pick it up)"
    }

    Say "installed. Next: rbuild init <ssh-host>; rbuild add ~\Code; rbuild init-shell powershell"
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
