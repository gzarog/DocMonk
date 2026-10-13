# Installs the native DocMonk CLI on Windows: downloads the
# release archive, verifies it against the release's SHA256SUMS (and the
# minisign signature over it, when a key is configured below and
# minisign.exe is on PATH), unpacks it under
# $env:DOCMONK_INSTALL_DIR\versions, and puts docmonk.exe on a per-user
# bin directory added to the user's PATH.
#
#   DOCMONK_VERSION        version to install (default: the latest release)
#   DOCMONK_INSTALL_DIR    default: %LOCALAPPDATA%\DocMonk
#   DOCMONK_BIN_DIR        default: <install dir>\bin
#   DOCMONK_HOME           default: %LOCALAPPDATA%\DocMonk
#   DOCMONK_DOWNLOAD_BASE  a URL or local directory holding the release
#                          assets (testing and mirrors)

$ErrorActionPreference = "Stop"

$Repo = "gzarog/DocMonk"
# The release-signing public key (minisign). Empty: checksums only.
$MinisignPubkey = ""
$Target = "x86_64-pc-windows-msvc"
$InstallDir = if ($env:DOCMONK_INSTALL_DIR) { $env:DOCMONK_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "DocMonk" }
$LayoutBin = Join-Path $InstallDir "bin"
$BinDir = if ($env:DOCMONK_BIN_DIR) { $env:DOCMONK_BIN_DIR } else { $LayoutBin }
$DocMonkHome = if ($env:DOCMONK_HOME) { $env:DOCMONK_HOME } else { Join-Path $env:LOCALAPPDATA "DocMonk" }

if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    throw "No DocMonk release for $($env:PROCESSOR_ARCHITECTURE) Windows; build from source (see README.md)."
}

function Get-Asset([string]$From, [string]$Dest) {
    if ($From -match '^https?://') {
        for ($i = 1; $i -le 4; $i++) {
            try { Invoke-WebRequest -UseBasicParsing -Uri $From -OutFile $Dest; return }
            catch { if ($i -eq 4) { throw "failed to download ${From}: $_" }; Start-Sleep -Seconds $i }
        }
    } else {
        if (-not (Test-Path $From)) { throw "missing $From" }
        Copy-Item $From $Dest
    }
}

$Version = $env:DOCMONK_VERSION
if (-not $Version) {
    $latest = Invoke-RestMethod -UseBasicParsing -Headers @{ Accept = "application/vnd.github+json" } `
        -Uri "https://api.github.com/repos/$Repo/releases/latest"
    $Version = "$($latest.tag_name)" -replace '^v', ''
    if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "latest release tag '$($latest.tag_name)' is not a MAJOR.MINOR.PATCH version" }
}
$Version = $Version -replace '^v', ''
if ($Version -notmatch '^[0-9][0-9A-Za-z.+-]*$') { throw "invalid version '$Version'" }
$Base = if ($env:DOCMONK_DOWNLOAD_BASE) { $env:DOCMONK_DOWNLOAD_BASE.TrimEnd('/', '\') } else { "https://github.com/$Repo/releases/download/v$Version" }
$Asset = "docmonk-$Version-$Target.zip"
$Sep = if ($Base -match '^https?://') { "/" } else { "\" }

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ("docmonk-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    Write-Host "Downloading DocMonk $Version ($Target)..."
    Get-Asset "$Base$Sep$Asset" (Join-Path $Tmp $Asset)
    Get-Asset "$Base${Sep}SHA256SUMS" (Join-Path $Tmp "SHA256SUMS")

    if ($MinisignPubkey) {
        if (Get-Command minisign -ErrorAction SilentlyContinue) {
            Get-Asset "$Base${Sep}SHA256SUMS.minisig" (Join-Path $Tmp "SHA256SUMS.minisig")
            & minisign -Vqm (Join-Path $Tmp "SHA256SUMS") -P $MinisignPubkey
            if ($LASTEXITCODE -ne 0) { throw "SHA256SUMS signature does not verify; refusing to install" }
            Write-Host "Signature verified."
        } else {
            Write-Warning "install minisign to also verify the release signature"
        }
    }

    $expected = $null
    foreach ($line in Get-Content (Join-Path $Tmp "SHA256SUMS")) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $Asset) { $expected = $parts[0].ToLower() }
    }
    if (-not $expected) { throw "$Asset is not listed in SHA256SUMS" }
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $Tmp $Asset)).Hash.ToLower()
    if ($actual -ne $expected) { throw "checksum mismatch for ${Asset}: expected $expected, got $actual; refusing to install" }
    Write-Host "Checksum verified."

    $Versions = Join-Path $InstallDir "versions"
    $Partial = Join-Path $Versions "$Version.partial"
    $Unpack = Join-Path $Tmp "unpack"
    New-Item -ItemType Directory -Force -Path $Versions | Out-Null
    if (Test-Path $Partial) { Remove-Item -Recurse -Force $Partial }
    Expand-Archive -Path (Join-Path $Tmp $Asset) -DestinationPath $Unpack
    $top = Get-ChildItem $Unpack -Directory | Select-Object -First 1
    Move-Item $top.FullName $Partial
    $exe = Join-Path $Partial "docmonk.exe"
    if (-not (Test-Path $exe)) { throw "$Asset does not contain docmonk.exe" }
    & $exe version | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "the downloaded binary does not run on this machine" }
    $VersionDir = Join-Path $Versions $Version
    if (Test-Path $VersionDir) { Remove-Item -Recurse -Force $VersionDir }
    Move-Item $Partial $VersionDir
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}

$Models = Join-Path $VersionDir "models"
if (Test-Path $Models) {
    $dest = Join-Path $DocMonkHome "models"
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    Copy-Item -Recurse -Force (Join-Path $Models "*") $dest
}

# Windows cannot replace a running executable but can rename it: the
# previous docmonk.exe becomes docmonk.exe.old (the same swap
# `docmonk update install` does).
New-Item -ItemType Directory -Force -Path $LayoutBin | Out-Null
$Exe = Join-Path $LayoutBin "docmonk.exe"
$OldExe = "$Exe.old"
if (Test-Path $OldExe) { Remove-Item -Force $OldExe -ErrorAction SilentlyContinue }
# Still there: an older exe is still running; park this one elsewhere.
if (Test-Path $OldExe) { $OldExe = "$Exe.old-$PID" }
if (Test-Path $Exe) { Move-Item -Force $Exe $OldExe }
Copy-Item (Join-Path $VersionDir "docmonk.exe") $Exe
Set-Content -NoNewline -Path (Join-Path $InstallDir "current") -Value $Version

$StatePath = Join-Path $InstallDir "install_state.json"
$previous = $null
if (Test-Path $StatePath) {
    $old = Get-Content $StatePath -Raw | ConvertFrom-Json
    $previous = if ($old.current -and $old.current -ne $Version) { $old.current } else { $old.previous }
}
# WriteAllText: UTF-8 without the BOM Windows PowerShell 5 would add.
[IO.File]::WriteAllText($StatePath, ([ordered]@{ current = $Version; previous = $previous } | ConvertTo-Json))

if ($BinDir -ne $LayoutBin) {
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    Set-Content -Encoding ASCII -Path (Join-Path $BinDir "docmonk.cmd") -Value "@`"$Exe`" %*"
}
Write-Host "DocMonk $Version installed: $Exe"

# `docmonk update` reads this to find the install it manages.
New-Item -ItemType Directory -Force -Path $DocMonkHome | Out-Null
$info = [ordered]@{
    install_method = "native"
    repository = $Repo
    install_dir = $InstallDir
    bin_dir = $BinDir
} | ConvertTo-Json
[IO.File]::WriteAllText((Join-Path $DocMonkHome "install_info.json"), $info)

$UserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if (-not (($UserPath -split ';') -contains $BinDir)) {
    $NewPath = if ($UserPath) { "$UserPath;$BinDir" } else { $BinDir }
    [Environment]::SetEnvironmentVariable("PATH", $NewPath, "User")
    Write-Host "Added $BinDir to your user PATH. Open a new terminal for this to take effect."
}

Write-Host ""
Write-Host "Get started:"
Write-Host "  docmonk init"
Write-Host "  docmonk source add <path>"
Write-Host "  docmonk index"
