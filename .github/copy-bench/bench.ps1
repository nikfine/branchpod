# Copies a synthetic tree shaped like node_modules (28 000 files, ~230 MB) with `bp up` and with the
# copy tools of the OS. Every tool gets a source written just before (cold: no process has read its
# files yet) and copies it a second time (warm). Prints a Markdown table.
$ErrorActionPreference = 'Stop'

$bp = Join-Path $PWD ('target/release/branchpod' + $(if ($IsWindows) { '.exe' } else { '' }))
$root = Join-Path $env:RUNNER_TEMP 'bench'
$main = Join-Path $root 'main'
$wt = Join-Path $root 'wt'
$src = Join-Path $main 'node_modules'
$env:BRANCHPOD_HOME = Join-Path $root 'home'

function Remove-Tree($path) {
    if (Test-Path -LiteralPath $path) { [IO.Directory]::Delete($path, $true) }
}

$generation = 0
# 2 800 packages of 10 files: 3 at the top, 4 in lib, 3 in lib/sub; 256 B to 16 KB each.
function New-Source {
    Remove-Tree $src
    $script:generation++
    $random = [Random]::new(42)
    $buffer = [byte[]]::new(16384)
    $random.NextBytes($buffer)
    $n = 0
    for ($p = 0; $p -lt 2800; $p++) {
        $pkg = Join-Path $src ('pkg-{0:d4}' -f $p)
        $lib = Join-Path $pkg 'lib'
        $sub = Join-Path $lib 'sub'
        [void][IO.Directory]::CreateDirectory($sub)
        foreach ($dir in @($pkg, $pkg, $pkg, $lib, $lib, $lib, $lib, $sub, $sub, $sub)) {
            $n++
            # Every file differs, also from those of an earlier source.
            [BitConverter]::GetBytes([long]($script:generation * 1000000 + $n)).CopyTo($buffer, 0)
            $size = $random.Next(256, 16384)
            $file = [IO.File]::Create((Join-Path $dir "f$n.js"))
            $file.Write($buffer, 0, $size)
            $file.Dispose()
        }
    }
}

function Measure-Seconds([scriptblock]$block) {
    $watch = [Diagnostics.Stopwatch]::StartNew()
    & $block
    $watch.Stop()
    [math]::Round($watch.Elapsed.TotalSeconds, 1)
}

function Invoke-Checked([string]$what) {
    if ($LASTEXITCODE -ne 0) { throw "$what failed with exit code $LASTEXITCODE" }
}

# The repository: the main checkout holds the source, `bp up` copies it into the linked worktree.
New-Item -ItemType Directory -Force $main | Out-Null
Set-Content -LiteralPath (Join-Path $main '.gitignore') "node_modules/"
Set-Content -LiteralPath (Join-Path $main 'branchpod.toml') "version = 1`n[[file]]`npath = 'node_modules'`nmode = 'copy'`n"
git -C $main init -q -b main; Invoke-Checked 'git init'
git -C $main add .gitignore branchpod.toml; Invoke-Checked 'git add'
git -C $main -c user.name=bench -c user.email=bench@example.invalid commit -q -m init; Invoke-Checked 'git commit'
git -C $main worktree add -q --detach $wt main; Invoke-Checked 'git worktree add'

$dest = Join-Path $wt 'out'
$tools = [ordered]@{}
$tools['bp up (copy)'] = {
    & $bp up $wt | Out-Null; Invoke-Checked 'bp up'
}
if ($IsLinux) {
    $tools['cp -a'] = { cp -a $src $dest; Invoke-Checked 'cp' }
}
if ($IsMacOS) {
    $tools['cp -R'] = { cp -R $src $dest; Invoke-Checked 'cp' }
    $tools['ditto'] = { ditto $src $dest; Invoke-Checked 'ditto' }
}
if ($IsWindows) {
    $tools['robocopy /MT:8'] = {
        robocopy $src $dest /E /MT:8 /NFL /NDL /NJH /NJS /NP | Out-Null
        if ($LASTEXITCODE -ge 8) { throw "robocopy failed with exit code $LASTEXITCODE" }
    }
}
$tools['.NET File.Copy, 1 thread'] = {
    foreach ($file in [IO.Directory]::EnumerateFiles($src, '*', 'AllDirectories')) {
        $to = $dest + $file.Substring($src.Length)
        [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($to))
        [IO.File]::Copy($file, $to)
    }
}

$rows = @()
$generate = @()
$down = @()
foreach ($name in $tools.Keys) {
    for ($round = 1; $round -le 2; $round++) {
        $generate += Measure-Seconds { New-Source }
        foreach ($state in 'cold', 'warm') {
            $seconds = Measure-Seconds $tools[$name]
            $rows += [pscustomobject]@{ Tool = $name; Round = $round; State = $state; Seconds = $seconds }
            if ($name -like 'bp up*') {
                $down += Measure-Seconds { & $bp down $wt | Out-Null; Invoke-Checked 'bp down' }
            } else {
                Remove-Tree $dest
            }
        }
    }
}

$os = if ($IsWindows) { 'Windows' } elseif ($IsMacOS) { 'macOS' } else { 'Linux' }
$lines = @("### $os", '', '| Tool | cold 1 | warm 1 | cold 2 | warm 2 |', '|---|---|---|---|---|')
foreach ($name in $tools.Keys) {
    $of = { param($r, $s) ($rows | Where-Object { $_.Tool -eq $name -and $_.Round -eq $r -and $_.State -eq $s }).Seconds }
    $lines += "| $name | $(& $of 1 'cold') s | $(& $of 1 'warm') s | $(& $of 2 'cold') s | $(& $of 2 'warm') s |"
}
$lines += ''
$lines += "Writing a source: $($generate -join ' s, ') s. ``bp down``: $($down -join ' s, ') s."
if ($IsWindows) {
    try {
        $mp = Get-MpComputerStatus
        $lines += "Defender: real-time protection $($mp.RealTimeProtectionEnabled), mode $($mp.AMRunningMode)."
    } catch {
        $lines += "Defender: status not available ($($_.Exception.Message))."
    }
}
$lines += "CPUs: $([Environment]::ProcessorCount)."
$text = $lines -join "`n"
$text
Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY $text
