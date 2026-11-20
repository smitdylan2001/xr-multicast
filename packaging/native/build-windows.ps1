# Build the native XR MultiCast app for Windows (x64): a portable zip and an Inno Setup installer.
#
#   pwsh packaging/native/build-windows.ps1 [-SkipInstaller]
#
# Needs: Visual Studio Build Tools (MSVC + Windows SDK), rustup, and Inno Setup 6 for the
# installer (installed with Chocolatey if ISCC.exe isn't found and choco is available).
# Bundles LGPL FFmpeg DLLs (BtbN build, pinned below) in ffmpeg\ for hardware decoding
# (D3D11VA/DXVA2) and H.265/AV1; OpenH264 is compiled in, so H.264 works without them.
# Output (dist-native\):
#   xr-multicast-<version>-windows-x64.zip
#   xr-multicast-<version>-windows-x64-setup.exe
param([switch]$SkipInstaller)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Root = (Resolve-Path "$PSScriptRoot\..\..").Path
$Version = (Select-String -Path "$Root\Cargo.toml" -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$Out = "$Root\dist-native"
$Target = 'x86_64-pc-windows-msvc'
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = "$Root\target" }

$Scrcpy = @{
  Url    = 'https://github.com/Genymobile/scrcpy/releases/download/v5.0.1/scrcpy-server-v5.0.1'
  Sha256 = '764eb6f79811d5211fe9df341120882ba9994c7a61b897d7bf3fb662e53bc536'
}
# FFmpeg 8.1.3 (libavcodec 62), LGPL-3.0 shared build from https://github.com/BtbN/FFmpeg-Builds
# (month-end autobuilds are kept long-term).
$FFmpeg = @{
  Version = 'n8.1.3-27-g8dca1586f4'
  Url     = 'https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-10-31-12-59/ffmpeg-n8.1.3-27-g8dca1586f4-win64-lgpl-shared-8.1.zip'
  Sha256  = '5cef7bc48e513f3cee7c881e8b2999d835541edb1846c3e89443a67f8f8556ab'
  Dlls    = @('avcodec-62.dll', 'avutil-60.dll', 'swresample-6.dll')
}

function Get-Verified($Url, $Sha256, $Dest) {
  if ((Test-Path $Dest) -and ((Get-FileHash -Algorithm SHA256 $Dest).Hash -eq $Sha256)) { return }
  New-Item -ItemType Directory -Force (Split-Path $Dest) | Out-Null
  Write-Host "downloading $Url"
  Invoke-WebRequest -Uri $Url -OutFile "$Dest.part" -UseBasicParsing
  $got = (Get-FileHash -Algorithm SHA256 "$Dest.part").Hash
  if ($got -ne $Sha256) {
    Remove-Item "$Dest.part"
    throw "checksum mismatch for $Url`: got $got, expected $Sha256"
  }
  Move-Item -Force "$Dest.part" $Dest
}

$Jar = $env:XRMC_SCRCPY_SERVER
if (-not $Jar) {
  $Jar = "$Root\vendor\scrcpy-server"
  Get-Verified $Scrcpy.Url $Scrcpy.Sha256 $Jar
}
$FFZip = "$Root\vendor\ffmpeg-$($FFmpeg.Version)-win64-lgpl-shared.zip"
Get-Verified $FFmpeg.Url $FFmpeg.Sha256 $FFZip

Write-Host "==> building $Target"
# Static CRT: no Visual C++ Redistributable needed on the target machine.
$env:RUSTFLAGS = "$env:RUSTFLAGS -C target-feature=+crt-static".Trim()
Push-Location $Root
try {
  rustup target add $Target | Out-Null
  cargo build --locked --release -p xrmc-desktop -p xrmc-cli --target $Target --features xrmc-decode/openh264-source
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally { Pop-Location }
$Bin = "$env:CARGO_TARGET_DIR\$Target\release"

$Name = "xr-multicast-$Version-windows-x64"
$Stage = "$Out\$Name"
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Force "$Stage\ffmpeg" | Out-Null
Copy-Item "$Bin\xr-multicast.exe", "$Bin\xrmc.exe" $Stage
Copy-Item $Jar "$Stage\scrcpy-server"
Copy-Item "$Root\LICENSE" "$Stage\LICENSE.txt"
if (Test-Path "$Root\THIRD_PARTY_NOTICES.md") { Copy-Item "$Root\THIRD_PARTY_NOTICES.md" $Stage }

Write-Host "==> FFmpeg $($FFmpeg.Version)"
$FFTmp = Join-Path ([IO.Path]::GetTempPath()) "xrmc-ffmpeg-$PID"
Expand-Archive -Force $FFZip $FFTmp
$FFDir = (Get-ChildItem $FFTmp -Directory | Select-Object -First 1).FullName
foreach ($d in $FFmpeg.Dlls) { Copy-Item "$FFDir\bin\$d" "$Stage\ffmpeg\" }
Copy-Item "$FFDir\LICENSE.txt" "$Stage\ffmpeg\LICENSE.txt"
Remove-Item -Recurse -Force $FFTmp
@"
FFmpeg $($FFmpeg.Version) (libavcodec, libavutil, libswresample), shared libraries
licensed under the GNU Lesser General Public License v3 (see LICENSE.txt here).

Build: BtbN FFmpeg-Builds, "win64-lgpl-shared" variant, unmodified:
  $($FFmpeg.Url)
  SHA-256 $($FFmpeg.Sha256.ToLower())
Source code of this FFmpeg version: https://github.com/FFmpeg/FFmpeg/tree/$($FFmpeg.Version -replace '^n[^-]+-\d+-g', '')
  (release branch tarballs: https://ffmpeg.org/download.html); build scripts:
  https://github.com/BtbN/FFmpeg-Builds

XR MultiCast loads these DLLs at runtime and works without them (H.264 via the
built-in OpenH264 decoder). You may replace them with another FFmpeg 5.x-8.x
build (libavcodec 59-62) with the same file names, or delete this folder.
"@ | Set-Content -Encoding UTF8 "$Stage\ffmpeg\README.txt"

Write-Host "==> zip"
$Zip = "$Out\$Name.zip"
if (Test-Path $Zip) { Remove-Item $Zip }
Compress-Archive -Path $Stage -DestinationPath $Zip -CompressionLevel Optimal
Write-Host ("    $Zip ({0:N1} MB)" -f ((Get-Item $Zip).Length / 1MB))

if (-not $SkipInstaller) {
  Write-Host "==> installer"
  $Iscc = @("${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe", "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
  if (-not $Iscc) {
    $cmd = Get-Command iscc -ErrorAction SilentlyContinue
    if ($cmd) { $Iscc = $cmd.Source }
  }
  if (-not $Iscc -and (Get-Command choco -ErrorAction SilentlyContinue)) {
    choco install innosetup -y --no-progress | Out-Host
    $Iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
  }
  if (-not $Iscc -or -not (Test-Path $Iscc)) { throw "Inno Setup 6 (ISCC.exe) not found; install it or use -SkipInstaller" }
  & $Iscc /Q "/DAppVersion=$Version" "/DSourceDir=$Stage" "/DOutputDir=$Out" "/DRepoRoot=$Root" "$Root\packaging\windows\xr-multicast.iss"
  if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
  $Setup = "$Out\$Name-setup.exe"
  Write-Host ("    $Setup ({0:N1} MB)" -f ((Get-Item $Setup).Length / 1MB))
}
