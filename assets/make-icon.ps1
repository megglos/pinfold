# Generates assets\pinfold.ico and assets\pinfold-paused.ico.
# A chip with four cores; the pinned half is lit. Every size is drawn
# natively on the pixel grid so the 16 px tray version stays crisp.
#
#   powershell -ExecutionPolicy Bypass -File assets\make-icon.ps1

Add-Type -AssemblyName System.Drawing
$ErrorActionPreference = 'Stop'

function New-RoundedPath([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    if ($r -le 0.5) { $p.AddRectangle((New-Object System.Drawing.RectangleF $x, $y, $w, $h)); return $p }
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

function Color([string]$hex) { [System.Drawing.ColorTranslator]::FromHtml($hex) }

function New-IconPng([int]$s, [bool]$paused) {
    $bmp = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.PixelOffsetMode = 'Half'
    $g.Clear([System.Drawing.Color]::Transparent)

    # Leads only where there are enough pixels for them.
    $leads = $s -ge 24
    $inset = if ($leads) { [math]::Round($s * 0.125) } else { [math]::Max(0, [math]::Round($s * 0.03)) }
    $body = $s - 2 * $inset

    if ($leads) {
        $lw = [math]::Max(2, [math]::Round($s * 0.07))      # lead thickness
        $ll = $inset + [math]::Round($s * 0.03)             # lead length (tucks under the body)
        $brush = New-Object System.Drawing.SolidBrush (Color '#A9B4C2')
        foreach ($f in 0.27, 0.5, 0.73) {
            $c = [math]::Round($s * $f - $lw / 2)
            foreach ($rect in @(
                @($c, 0, $lw, $ll), @($c, ($s - $ll), $lw, $ll),
                @(0, $c, $ll, $lw), @(($s - $ll), $c, $ll, $lw))) {
                $path = New-RoundedPath $rect[0] $rect[1] $rect[2] $rect[3] ([math]::Min($lw, $ll) * 0.3)
                $g.FillPath($brush, $path); $path.Dispose()
            }
        }
        $brush.Dispose()
    }

    # Chip body
    $bodyRect = New-Object System.Drawing.RectangleF $inset, $inset, $body, $body
    $path = New-RoundedPath $inset $inset $body $body ($body * 0.18)
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush $bodyRect, (Color '#3A4656'), (Color '#1A2029'), 90
    $g.FillPath($grad, $path); $grad.Dispose()
    if ($s -ge 32) {
        $pen = New-Object System.Drawing.Pen (Color '#55657A'), ([math]::Max(1, $s / 64))
        $g.DrawPath($pen, $path); $pen.Dispose()
    }
    $path.Dispose()

    # 2 x 2 cores: left column lit (the pinned half), right column dim.
    $pad = [math]::Max(2, [math]::Round($body * 0.17))
    $gap = [math]::Max(1, [math]::Round($body * 0.11))
    if ($s -le 16) { $gap = 2 }
    $core = [math]::Floor(($body - 2 * $pad - $gap) / 2)
    $pad = ($body - 2 * $core - $gap) / 2                   # re-centre after rounding
    $lit = if ($paused) { '#B4BDC9', '#8792A0' } else { '#FFC53D', '#FF8A1F' }
    $dim = '#5B697B', '#465263'
    for ($row = 0; $row -lt 2; $row++) {
        for ($col = 0; $col -lt 2; $col++) {
            $x = $inset + $pad + $col * ($core + $gap)
            $y = $inset + $pad + $row * ($core + $gap)
            $colors = if ($col -eq 0) { $lit } else { $dim }
            $rect = New-Object System.Drawing.RectangleF $x, $y, $core, $core
            $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rect, (Color $colors[0]), (Color $colors[1]), 90
            $path = New-RoundedPath $x $y $core $core ($core * 0.22)
            $g.FillPath($brush, $path)
            $brush.Dispose(); $path.Dispose()
        }
    }

    $g.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    return ,$ms.ToArray()
}

function Write-Ico([string]$path, [bool]$paused) {
    $sizes = 16, 20, 24, 32, 40, 48, 64, 256
    $images = foreach ($s in $sizes) { ,(New-IconPng $s $paused) }
    $fs = [System.IO.File]::Create($path)
    $w = New-Object System.IO.BinaryWriter $fs
    $w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        $dim = if ($sizes[$i] -ge 256) { 0 } else { $sizes[$i] }
        $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
        $w.Write([uint16]1); $w.Write([uint16]32)
        $w.Write([uint32]$images[$i].Length); $w.Write([uint32]$offset)
        $offset += $images[$i].Length
    }
    foreach ($img in $images) { $w.Write($img) }
    $w.Dispose(); $fs.Dispose()
    "wrote $path"
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
Write-Ico (Join-Path $here 'pinfold.ico') $false
Write-Ico (Join-Path $here 'pinfold-paused.ico') $true

# Optional preview sheet: -Preview <file.png>
if ($args.Count -eq 2 -and $args[0] -eq '-Preview') {
    $sheet = New-Object System.Drawing.Bitmap 760, 300
    $g = [System.Drawing.Graphics]::FromImage($sheet)
    $g.Clear((Color '#F3F3F3'))
    $g.FillRectangle((New-Object System.Drawing.SolidBrush (Color '#202020')), 380, 0, 380, 300)
    $g.InterpolationMode = 'NearestNeighbor'; $g.PixelOffsetMode = 'Half'
    foreach ($side in 0, 1) {
        $ox = $side * 380 + 10
        $img = [System.Drawing.Image]::FromStream((New-Object System.IO.MemoryStream (,(New-IconPng 256 $false))))
        $g.DrawImage($img, $ox, 10, 180, 180)
        $x = $ox + 195
        foreach ($s in 48, 32, 24, 16) {
            foreach ($p in $false, $true) {
                $img = [System.Drawing.Image]::FromStream((New-Object System.IO.MemoryStream (,(New-IconPng $s $p))))
                $y = if ($p) { 80 } else { 10 }
                $g.DrawImage($img, $x, $y, $s, $s)
            }
            $x += $s + 10
        }
        # 16 px magnified 8x to judge the tray version
        $img = [System.Drawing.Image]::FromStream((New-Object System.IO.MemoryStream (,(New-IconPng 16 $false))))
        $g.DrawImage($img, $ox + 195, 150, 128, 128)
    }
    $g.Dispose(); $sheet.Save($args[1]); $sheet.Dispose()
    "wrote $($args[1])"
}
