# What CopyFile2 and MoveFileExW (without MOVEFILE_REPLACE_EXISTING) do with a link at the destination.
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Copy2 {
    [StructLayout(LayoutKind.Sequential)]
    public struct Params {
        public uint dwSize;
        public uint dwCopyFlags;
        public IntPtr pfCancel;
        public IntPtr pProgressRoutine;
        public IntPtr pvCallbackContext;
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    static extern int CopyFile2(string from, string to, ref Params p);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool MoveFileExW(string from, string to, uint flags);
    public static int Copy(string from, string to, uint flags) {
        var p = new Params();
        p.dwSize = (uint)Marshal.SizeOf(typeof(Params));
        p.dwCopyFlags = flags;
        return CopyFile2(from, to, ref p);
    }
    public static int Move(string from, string to) {
        return MoveFileExW(from, to, 0) ? 0 : Marshal.GetLastWin32Error();
    }
}
'@
$FAIL = 0x1; $SYMLINK = 0x800
$root = Join-Path $env:RUNNER_TEMP 'links'
function Get-State($dest, $target) {
    $item = Get-Item -LiteralPath $dest -Force -ErrorAction SilentlyContinue
    $d = if (-not $item) { 'gone' } elseif ($item.LinkType) { "link" } else { "file: $((Get-Content -LiteralPath $dest -Raw).Trim())" }
    $t = if (Test-Path -LiteralPath $target -PathType Container) {
        "directory with: $((Get-ChildItem -LiteralPath $target -Force).Name -join ', ')"
    } elseif (Test-Path -LiteralPath $target) {
        "exists: $((Get-Content -LiteralPath $target -Raw).Trim())"
    } else { 'absent' }
    "$d | $t"
}
$n = 0
function New-Case($case, $kind) {
    # A directory of its own for every case: nothing is cleaned up in between.
    $script:n++
    $script:root = Join-Path $env:RUNNER_TEMP "links-$script:n"
    New-Item -ItemType Directory $root | Out-Null
    Set-Content -LiteralPath (Join-Path $root 'src.txt') 'from main'
    $target = Join-Path $root 'target'
    if ($case -eq 'live') {
        if ($kind -eq 'junction') { New-Item -ItemType Directory $target | Out-Null } else { Set-Content -LiteralPath $target 'keep me' }
    }
    $type = if ($kind -eq 'junction') { 'Junction' } else { 'SymbolicLink' }
    if ($kind -eq 'junction' -and $case -eq 'dangling') {
        New-Item -ItemType Directory $target | Out-Null
        New-Item -ItemType Junction -Path (Join-Path $root 'dest.txt') -Target $target | Out-Null
        [IO.Directory]::Delete($target)
    } else {
        New-Item -ItemType $type -Path (Join-Path $root 'dest.txt') -Target $target | Out-Null
    }
}
$lines = @('| Destination | Operation | Result | Destination after | Link target after |', '|---|---|---|---|---|')
foreach ($kind in 'symlink', 'junction') {
    foreach ($case in 'dangling', 'live') {
        New-Case $case $kind
        $src = Join-Path $root 'src.txt'; $dest = Join-Path $root 'dest.txt'; $target = Join-Path $root 'target'
        $hr = [Copy2]::Copy($src, $dest, [uint32]($FAIL -bor $SYMLINK))
        $lines += "| $case $kind | CopyFile2 FAIL_IF_EXISTS+COPY_SYMLINK | 0x$('{0:x8}' -f $hr) | $(Get-State $dest $target) |"
        New-Case $case $kind
        $src = Join-Path $root 'src.txt'; $dest = Join-Path $root 'dest.txt'; $target = Join-Path $root 'target'
        $tmp = Join-Path $root 'tmp-1234.txt'
        $hr = [Copy2]::Copy($src, $tmp, [uint32]$FAIL)
        $err = [Copy2]::Move($tmp, $dest)
        $tmpLeft = if (Test-Path -LiteralPath $tmp) { 'temp left' } else { 'temp gone' }
        $lines += "| $case $kind | CopyFile2 to temp (0x$('{0:x8}' -f $hr)), MoveFileExW flags 0 | error $err, $tmpLeft | $(Get-State $dest $target) |"
    }
}
$text = "### CopyFile2 / MoveFileExW and a link at the destination`n`n" + ($lines -join "`n") + "`n`nOS: $([Environment]::OSVersion.VersionString)"
$text
Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY $text
exit 0
