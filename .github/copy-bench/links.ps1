# What CopyFile2 does with a symbolic link at the destination, for each set of flags.
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
    public static int Run(string from, string to, uint flags) {
        var p = new Params();
        p.dwSize = (uint)Marshal.SizeOf(typeof(Params));
        p.dwCopyFlags = flags;
        return CopyFile2(from, to, ref p);
    }
}
'@
$FAIL = 0x1; $SYMLINK = 0x800
$root = Join-Path $env:RUNNER_TEMP 'links'
$lines = @('| Destination | Flags | HRESULT | Destination after | Link target after |', '|---|---|---|---|---|')
foreach ($case in 'dangling', 'live') {
    foreach ($flags in @($FAIL, ($FAIL -bor $SYMLINK))) {
        if (Test-Path $root) { [IO.Directory]::Delete($root, $true) }
        New-Item -ItemType Directory $root | Out-Null
        $src = Join-Path $root 'src.txt'; Set-Content -LiteralPath $src 'from main'
        $target = Join-Path $root 'target.txt'
        if ($case -eq 'live') { Set-Content -LiteralPath $target 'keep me' }
        $dest = Join-Path $root 'dest.txt'
        New-Item -ItemType SymbolicLink -Path $dest -Target $target | Out-Null
        $hr = [Copy2]::Run($src, $dest, [uint32]$flags)
        $item = Get-Item -LiteralPath $dest -Force -ErrorAction SilentlyContinue
        $destAfter = if (-not $item) { 'gone' } elseif ($item.LinkType) { "link -> $($item.Target)" } else { "file: $(Get-Content -LiteralPath $dest -Raw)".Trim() }
        $targetAfter = if (Test-Path -LiteralPath $target) { "exists: $((Get-Content -LiteralPath $target -Raw).Trim())" } else { 'absent' }
        $name = if ($flags -band $SYMLINK) { 'FAIL_IF_EXISTS + COPY_SYMLINK' } else { 'FAIL_IF_EXISTS' }
        $lines += "| $case link | $name | 0x$('{0:x8}' -f $hr) | $destAfter | $targetAfter |"
    }
}
$text = "### CopyFile2 and a link at the destination`n`n" + ($lines -join "`n") + "`n`nOS: $([Environment]::OSVersion.VersionString)"
$text
Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY $text
exit 0
