# The Windows binary

A [release](https://github.com/nikfine/branchpod/releases) has an executable for x86_64 Windows and a file with its
SHA256. Both are made by GitHub Actions from the tagged commit ([workflow](../.github/workflows/windows-build.yml)).
The executable needs no installer, no Visual C++ runtime and no administrator rights.

## Check the hash

In PowerShell, in the directory with both files, with the name you downloaded:

```powershell
$name = "branchpod-v0.1.0-x86_64-pc-windows-msvc.exe"
(Get-FileHash $name).Hash -eq (Get-Content "$name.sha256").Split(" ")[0]
```

`True` means that the file arrived as it was published. It does not prove who made it: whoever could replace the
executable in the release could replace the hash next to it.

Then rename the file to `branchpod.exe`, put it in a directory on `PATH` and ask it for its version:

```
branchpod --version
```

## Warnings

The executable is not signed, and few people have downloaded it. So Windows SmartScreen may warn about an unknown
publisher, and a heuristic antivirus may flag it. A warning does not tell whether the file is harmless.

- If the hash does not match, delete the file.
- If an antivirus blocks the file or flags it, do not run it. [Report it](https://github.com/nikfine/branchpod/issues)
  with the name of the antivirus, its verdict and the SHA256 of the file. Look through what you paste for user names
  and paths.
- You can also build it from source, see the [README](../README.md#installation).
