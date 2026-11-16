# Compares frame-time captures, e.g. one game session pinned to "Cache"
# and one pinned to "Frequency".
#
# Input: CSV files written by PresentMon (or tools built on it, such as
# CapFrameX exports). The frame time column is found by name:
# FrameTime, MsBetweenPresents or msBetweenPresents.
#
#   powershell -ExecutionPolicy Bypass -File scripts\compare-frametimes.ps1 cache.csv frequency.csv
#   ... -SkipSeconds 10      ignore the first seconds of each capture (loading, alt-tab)
#   ... -Process longshot.exe  keep only rows of this process (captures of everything)
#
# Lower frame times are better. The first file is the baseline for the
# difference columns.

param(
    [Parameter(Mandatory, Position = 0, ValueFromRemainingArguments)]
    [string[]]$Files,
    [double]$SkipSeconds = 0,
    [string]$Process
)
$ErrorActionPreference = 'Stop'

function Get-Percentile([double[]]$sorted, [double]$p) {
    # Linear interpolation between closest ranks; $sorted is ascending.
    if ($sorted.Count -eq 1) { return $sorted[0] }
    $rank = ($p / 100) * ($sorted.Count - 1)
    $lo = [math]::Floor($rank); $hi = [math]::Ceiling($rank)
    return $sorted[$lo] + ($sorted[$hi] - $sorted[$lo]) * ($rank - $lo)
}

function Read-Capture([string]$path) {
    $rows = Import-Csv $path
    if (-not $rows) { throw "$path has no rows" }
    $cols = $rows[0].PSObject.Properties.Name
    $col = @('FrameTime', 'MsBetweenPresents', 'msBetweenPresents') | Where-Object { $cols -contains $_ } | Select-Object -First 1
    if (-not $col) { throw "$path has no frame time column (FrameTime or MsBetweenPresents). Columns: $($cols -join ', ')" }
    $procCol = @('Application', 'ProcessName') | Where-Object { $cols -contains $_ } | Select-Object -First 1
    if ($Process -and $procCol) { $rows = @($rows | Where-Object { $_.$procCol -eq $Process }) }
    $inv = [Globalization.CultureInfo]::InvariantCulture
    $times = New-Object System.Collections.Generic.List[double]
    $elapsed = 0.0
    foreach ($r in $rows) {
        $v = 0.0
        if (-not [double]::TryParse($r.$col, [Globalization.NumberStyles]::Float, $inv, [ref]$v)) { continue }
        if ($v -le 0) { continue }
        $elapsed += $v
        if ($elapsed / 1000 -gt $SkipSeconds) { $times.Add($v) }
    }
    if ($times.Count -lt 2) { throw "$path has fewer than 2 usable frames" }
    $sorted = $times.ToArray(); [array]::Sort($sorted)
    $sum = ($sorted | Measure-Object -Sum).Sum
    # "1 % low": average FPS over the slowest 1 % of frames.
    $n1 = [math]::Max(1, [int][math]::Ceiling($sorted.Count * 0.01))
    $n01 = [math]::Max(1, [int][math]::Ceiling($sorted.Count * 0.001))
    $slow1 = ($sorted[($sorted.Count - $n1)..($sorted.Count - 1)] | Measure-Object -Average).Average
    $slow01 = ($sorted[($sorted.Count - $n01)..($sorted.Count - 1)] | Measure-Object -Average).Average
    [ordered]@{
        'frames'              = $sorted.Count
        'duration s'          = $sum / 1000
        'avg FPS'             = 1000 * $sorted.Count / $sum
        '1% low FPS'          = 1000 / $slow1
        '0.1% low FPS'        = 1000 / $slow01
        'frame time avg ms'   = $sum / $sorted.Count
        'frame time p50 ms'   = Get-Percentile $sorted 50
        'frame time p95 ms'   = Get-Percentile $sorted 95
        'frame time p99 ms'   = Get-Percentile $sorted 99
        'frame time p99.9 ms' = Get-Percentile $sorted 99.9
        'frame time max ms'   = $sorted[-1]
    }
}

$captures = foreach ($f in $Files) { [pscustomobject]@{ Name = [IO.Path]::GetFileNameWithoutExtension($f); Stats = Read-Capture $f } }
$base = $captures[0]
$rows = foreach ($metric in $base.Stats.Keys) {
    $row = [ordered]@{ metric = $metric }
    foreach ($c in $captures) {
        $v = $c.Stats[$metric]
        $row[$c.Name] = if ($metric -eq 'frames') { '{0:N0}' -f $v } else { '{0:N2}' -f $v }
        if ($c.Name -ne $base.Name) {
            # Every row carries the column, or the table would drop it.
            $row["$($c.Name) vs $($base.Name)"] = if ($metric -in 'frames', 'duration s') { '' } else {
                '{0:+0.0;-0.0}%' -f (100 * ($v - $base.Stats[$metric]) / $base.Stats[$metric])
            }
        }
    }
    [pscustomobject]$row
}
$rows | Format-Table -AutoSize
"FPS rows: higher is better. Frame time rows: lower is better."
"Differences under about 2-3 % are within run-to-run noise; repeat each setting to be sure."
