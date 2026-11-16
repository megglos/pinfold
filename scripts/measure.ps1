# A7: idle resource measurement. 5 rules, 2 matched processes, 10 minutes.
# Runs a private tray instance (own config dir) next to the installed one.
param([int]$Seconds = 600)
$repo = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$exe = Join-Path $repo 'target\release\Pinfold.exe'
$dir = Join-Path $env:TEMP "pinfold-measure-$PID"
New-Item -ItemType Directory -Force "$dir\t" | Out-Null
$names = 1..2 | ForEach-Object { "pf-measure-$PID-$_.exe" }
$rules = @($names + 'not-running-a.exe','not-running-b.exe','not-running-c.exe') |
    ForEach-Object { "{ `"match`": `"$_`", `"profile`": `"One`" }" }
"{ `"pollIntervalMs`": 1000, `"profiles`": [ { `"name`": `"One`", `"cpus`": `"0`" } ], `"rules`": [ $($rules -join ', ') ] }" |
    Set-Content -Encoding ascii "$dir\config.json"
$targets = foreach ($n in $names) {
    Copy-Item $exe "$dir\t\$n"
    $env:PINFOLD_DIR = "$dir\t\cfg"; Start-Process "$dir\t\$n" -ArgumentList run -WindowStyle Hidden -PassThru
}
$env:PINFOLD_DIR = $dir
$p = Start-Process $exe -PassThru
Start-Sleep 5   # startup, first scan, both targets pinned
function Snap { $p.Refresh(); $c = Get-CimInstance Win32_Process -Filter "ProcessId=$($p.Id)"
    [pscustomobject]@{ Cpu = $p.TotalProcessorTime.TotalMilliseconds; Private = $p.PrivateMemorySize64
        WorkingSet = $p.WorkingSet64; Threads = $p.Threads.Count; Writes = $c.WriteOperationCount; WriteBytes = $c.WriteTransferCount } }
$log0 = (Get-Item "$dir\pinfold.log").Length
$a = Snap; Start-Sleep $Seconds; $b = Snap
$pinned = @($targets | ForEach-Object { $_.Refresh(); [int64]$_.ProcessorAffinity })
$cpuMs = $b.Cpu - $a.Cpu
"measured over $Seconds s, 5 rules, matched processes pinned: $(@($pinned | Where-Object { $_ -eq 1 }).Count) of 2"
"cpu time:        {0:N0} ms  = {1:N4} % of one core" -f $cpuMs, ($cpuMs / ($Seconds * 10))
"private memory:  {0:N2} MB (start {1:N2} MB)" -f ($b.Private/1MB), ($a.Private/1MB)
"working set:     {0:N2} MB" -f ($b.WorkingSet/1MB)
"threads:         $($b.Threads)"
"disk writes:     $($b.Writes - $a.Writes) operations, $($b.WriteBytes - $a.WriteBytes) bytes; log grew $((Get-Item "$dir\pinfold.log").Length - $log0) bytes"
"executable size: {0:N0} bytes" -f (Get-Item $exe).Length
& $exe quit | Out-Null
$targets | ForEach-Object { Stop-Process -Id $_.Id -ErrorAction SilentlyContinue }
Start-Sleep 1; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
