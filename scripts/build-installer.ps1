# Builds the tacoterm installer (target\installer\tacoterm-<version>-setup.exe)
# from a release build with the bundled console beside it:
#
#   cargo build --release
#   .\scripts\get-conpty.ps1
#   .\scripts\build-installer.ps1
#
# tacoshell.exe is included when it is in the same folder. Needs Inno Setup 6
# (installed with winget or Chocolatey if it isn't there).
param(
    [string]$Source = "target\release"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value

function Find-Iscc {
    $cmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $candidates = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    )
    $candidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
}

$iscc = Find-Iscc
if (-not $iscc) {
    Write-Host "Inno Setup not found; installing it"
    if (Get-Command choco -ErrorAction SilentlyContinue) {
        choco install innosetup -y --no-progress | Out-Host
    } else {
        winget install --id JRSoftware.InnoSetup -e --silent --accept-source-agreements --accept-package-agreements | Out-Host
    }
    $iscc = Find-Iscc
    if (-not $iscc) { throw "Inno Setup (ISCC.exe) is needed to build the installer" }
}

$src = (Resolve-Path (Join-Path $root $Source)).Path
foreach ($f in "tacoterm.exe", "conpty.dll", "OpenConsole.exe") {
    if (-not (Test-Path (Join-Path $src $f))) {
        throw "$f is missing from $src (cargo build --release, then scripts\get-conpty.ps1)"
    }
}
if (Test-Path (Join-Path $src "tacoshell.exe")) {
    Write-Host "Including tacoshell.exe"
}
$out = Join-Path $root "target\installer"
& $iscc "/DAppVersion=$version" "/DSourceDir=$src" "/DOutputDir=$out" (Join-Path $root "installer\tacoterm.iss")
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }
Get-ChildItem $out -Filter "*.exe" | ForEach-Object { Write-Host "Built $($_.FullName)" }
