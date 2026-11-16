# Builds Pinfold and installs it to %LocalAppData%\Programs\Pinfold.
# Stops a running instance first (also an elevated one) and starts the new
# one afterwards, elevated through the logon task when that exists.
#
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1 [-Autostart] [-NoStart]

param(
    [switch]$Autostart,   # turn "Start with Windows" on (one UAC prompt for the elevated variant)
    [switch]$NoStart      # do not start Pinfold after installing
)
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$built = Join-Path $repo 'target\release\Pinfold.exe'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\Pinfold'
$exe = Join-Path $dest 'Pinfold.exe'

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

# Ask the running instance to exit. This also reaches an elevated instance,
# which an unelevated Stop-Process could not touch. (Piping makes PowerShell
# wait for the GUI-subsystem exe.)
& $built quit | Out-Host
# Fallback for versions that predate the quit command.
Get-Process Pinfold -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $exe } |
    ForEach-Object {
        try { Stop-Process -Id $_.Id -ErrorAction Stop; $_.WaitForExit(5000) | Out-Null }
        catch { throw "Pinfold (pid $($_.Id)) is running elevated and did not quit; exit it from the tray menu and run this script again." }
    }

New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item $built $exe -Force
"installed $exe ($((Get-Item $exe).Length) bytes)"

if ($Autostart) {
    & $exe autostart on | Out-Host
}

if (-not $NoStart) {
    # Prefer the logon task: it starts Pinfold elevated without a prompt.
    schtasks.exe /Query /TN Pinfold 2>$null | Out-Null
    if ($LASTEXITCODE -eq 0) {
        schtasks.exe /Run /TN Pinfold | Out-Null
        "started through the logon task (elevated)"
    } else {
        Start-Process $exe
        "started"
    }
}
& $exe autostart | Out-Host
