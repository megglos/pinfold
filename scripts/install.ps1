# Builds Pinfold and installs it to %LocalAppData%\Programs\Pinfold.
# Stops a running instance first, starts the new one afterwards, and keeps
# an existing "Start with Windows" entry pointing at the installed exe.
#
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1 [-Autostart] [-NoStart]

param(
    [switch]$Autostart,   # create the "Start with Windows" entry if missing
    [switch]$NoStart      # do not start Pinfold after installing
)
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$dest = Join-Path $env:LOCALAPPDATA 'Programs\Pinfold'
$exe = Join-Path $dest 'Pinfold.exe'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $env:PATH += ";$env:USERPROFILE\.cargo\bin"
}

Push-Location $repo
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    Pop-Location
}

# Only the background instance installed at the destination is stopped.
Get-Process Pinfold -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $exe } |
    ForEach-Object { Stop-Process -Id $_.Id; $_.WaitForExit(5000) | Out-Null }

New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item (Join-Path $repo 'target\release\Pinfold.exe') $exe -Force
"installed $exe ($((Get-Item $exe).Length) bytes)"

$current = (Get-ItemProperty $runKey -ErrorAction SilentlyContinue).Pinfold
if ($Autostart -or $current) {
    Set-ItemProperty -Path $runKey -Name Pinfold -Value "`"$exe`""
    "autostart: $exe"
}

if (-not $NoStart) {
    Start-Process $exe
    "started"
}
