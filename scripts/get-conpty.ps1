# Puts Windows Terminal's console (conpty.dll + OpenConsole.exe) next to
# tacoterm.exe. tacoterm uses it when it's there: unlike the console built
# into Windows, it passes pictures (kitty graphics, sixel, iTerm2) and other
# sequences straight through to the terminal.
#
#   .\scripts\get-conpty.ps1                       # into target\release
#   .\scripts\get-conpty.ps1 -Dest target\debug
#   .\scripts\get-conpty.ps1 -Version 1.25.260930003
#
# It comes from Microsoft's Microsoft.Windows.Console.ConPTY NuGet package
# (MIT licensed, like Windows Terminal).
param(
    [string]$Dest = "target\release",
    [string]$Version = ""
)
$ErrorActionPreference = "Stop"
$feed = "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty"
if (-not $Version) {
    # The newest release that isn't a preview.
    $Version = (Invoke-RestMethod "$feed/index.json").versions |
        Where-Object { $_ -notmatch '-' } |
        Select-Object -Last 1
}
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
$work = Join-Path ([IO.Path]::GetTempPath()) "tacoterm-conpty-$Version"
$zip = "$work.zip"
Invoke-WebRequest "$feed/$Version/microsoft.windows.console.conpty.$Version.nupkg" -OutFile $zip
Expand-Archive $zip $work -Force
New-Item -ItemType Directory -Force $Dest | Out-Null
Copy-Item (Join-Path $work "runtimes\win-$arch\native\conpty.dll") $Dest -Force
Copy-Item (Join-Path $work "build\native\runtimes\$arch\OpenConsole.exe") $Dest -Force
Remove-Item $zip, $work -Recurse -Force
Write-Host "Console $Version ($arch) is in $Dest"
