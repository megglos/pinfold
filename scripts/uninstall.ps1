# Removes Pinfold: "Start with Windows" (logon task and Run entry), the
# running instance and %LocalAppData%\Programs\Pinfold. With -RemoveData,
# also removes %AppData%\Pinfold (config, log, benchmark reports).
#
#   powershell -ExecutionPolicy Bypass -File scripts\uninstall.ps1 [-RemoveData]

param(
    [switch]$RemoveData   # also delete the config, the log and the benchmark reports
)
$ErrorActionPreference = 'Stop'

$dest = Join-Path $env:LOCALAPPDATA 'Programs\Pinfold'
$exe = Join-Path $dest 'Pinfold.exe'
$data = Join-Path $env:APPDATA 'Pinfold'

if (Test-Path $exe) {
    # Removes the logon task (one UAC prompt) and the Run entry.
    & $exe autostart off | Out-Host
    # Stops the running instance, also an elevated one. (The pipe makes
    # PowerShell wait for the GUI exe.)
    & $exe quit | Out-Host
} else {
    "Pinfold.exe is not in $dest"
}

if (Get-Process Pinfold -ErrorAction SilentlyContinue) {
    throw "Pinfold still runs. Select Exit in its tray menu and run this script again."
}

if (Test-Path $dest) {
    Remove-Item -Recurse -Force $dest
    "removed $dest"
}

if ($RemoveData) {
    if (Test-Path $data) {
        Remove-Item -Recurse -Force $data
        "removed $data"
    }
} elseif (Test-Path $data) {
    "kept $data (config, log, benchmark reports). Use -RemoveData to remove it."
}

"Programs that Pinfold pinned keep their cores until they start again."
