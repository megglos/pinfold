# Writes the release notes for a version:
#   1. the section of the version from CHANGELOG.md,
#   2. the commits since the previous tag,
#   3. the SHA-256 of each release file.
#
#   powershell -File scripts\release-notes.ps1 -Version 1.0.0-beta1 [-Files a.zip,b.exe] [-Out notes.md]

param(
    [Parameter(Mandatory)][string]$Version,
    [string[]]$Files = @(),
    [string]$Out = ''
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)

# 1. Section of the version: from "## <version>" to the next "## ".
$lines = Get-Content (Join-Path $repo 'CHANGELOG.md') -Encoding UTF8
$start = -1
for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match "^##\s+$([regex]::Escape($Version))(\s|$)") { $start = $i; break }
}
if ($start -lt 0) { throw "CHANGELOG.md has no section '## $Version'" }
$section = @()
for ($i = $start + 1; $i -lt $lines.Count -and $lines[$i] -notmatch '^##\s'; $i++) {
    # One level up: "### Install" becomes "## Install" in the release.
    $section += ($lines[$i] -replace '^###', '##')
}

# 2. Commits since the previous tag (all commits for the first release).
# Windows PowerShell stops on git's stderr output under 'Stop': use 'Continue'.
function Invoke-Git { $ErrorActionPreference = 'Continue'; $out = & git.exe -C $repo @args 2>$null; ,@($out) }
$tag = "v$Version"
$end = if ((Invoke-Git tag --list $tag).Count -gt 0) { $tag } else { 'HEAD' }
$previous = (Invoke-Git describe --tags --abbrev=0 --match 'v*' "$end^") | Select-Object -First 1
$range = if ($previous) { "$previous..$end" } else { $end }
$commits = Invoke-Git log --reverse --no-merges --format='%h %s' $range
if ($commits.Count -eq 0) { throw "no commits in $range" }

$notes = New-Object System.Collections.Generic.List[string]
$section | ForEach-Object { $notes.Add($_) }
while ($notes.Count -gt 0 -and $notes[0].Trim() -eq '') { $notes.RemoveAt(0) }
while ($notes.Count -gt 0 -and $notes[$notes.Count - 1].Trim() -eq '') { $notes.RemoveAt($notes.Count - 1) }
$notes.Add('')
$notes.Add($(if ($previous) { "## Changes since $previous" } else { '## All changes' }))
$notes.Add('')
foreach ($c in $commits) {
    $sha, $subject = $c -split ' ', 2
    $notes.Add("- $subject ($sha)")
}

# 3. Checksums.
if ($Files.Count -gt 0) {
    $notes.Add('')
    $notes.Add('## SHA-256')
    $notes.Add('')
    $notes.Add('```')
    foreach ($f in $Files) {
        $hash = (Get-FileHash -Algorithm SHA256 $f).Hash.ToLower()
        $notes.Add("$hash  $(Split-Path -Leaf $f)")
    }
    $notes.Add('```')
}

$text = ($notes -join "`n") + "`n"
if ($Out) {
    [IO.File]::WriteAllText($Out, $text, (New-Object Text.UTF8Encoding $false))
    "wrote $Out"
} else {
    $text
}
