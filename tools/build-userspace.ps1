# Build the freestanding SketchOS 0.0.6 user ELF images.
#
# Each program is one no_std Rust source file that includes the shared
# userspace/runtime.rs. Images are static ET_EXEC files linked at 0x400000 and
# are embedded into the kernel with include_bytes!.
#
# Usage:
#   .\tools\build-userspace.ps1

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

function Invoke-Rustc {
    param([string[]]$RustcArgs)

    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & rustc @RustcArgs 2>&1 | ForEach-Object { Write-Host $_ }
    $code = $LASTEXITCODE
    $ErrorActionPreference = $previous
    if ($code -ne 0) {
        throw "rustc failed ($code): rustc $($RustcArgs -join ' ')"
    }
}

try {
    $outDir = Join-Path $root 'userspace/build'
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null

    $target = 'aarch64-unknown-none'
    $programs = @(
        @{ Name = 'init'; Source = 'userspace/init.rs'; Output = 'init.elf' },
        @{ Name = 'ipc_recv'; Source = 'userspace/ipc_recv.rs'; Output = 'ipc-recv.elf' },
        @{ Name = 'ipc_send'; Source = 'userspace/ipc_send.rs'; Output = 'ipc-send.elf' },
        @{ Name = 'faulter'; Source = 'userspace/faulter.rs'; Output = 'faulter.elf' },
        @{ Name = 'threads'; Source = 'userspace/threads.rs'; Output = 'threads.elf' },
        @{ Name = 'latency'; Source = 'userspace/latency.rs'; Output = 'latency.elf' }
    )

    foreach ($program in $programs) {
        Write-Host "==> Building user image $($program.Name)"
        $output = Join-Path $outDir $program.Output
        Invoke-Rustc @(
            '--edition', '2021',
            '--crate-name', $program.Name,
            '--crate-type', 'bin',
            '--target', $target,
            '-O',
            '-C', 'panic=abort',
            '-C', 'debuginfo=0',
            '-C', 'relocation-model=static',
            '-C', 'link-arg=-Tuserspace/user.ld',
            '-C', 'link-arg=-z',
            '-C', 'link-arg=max-page-size=4096',
            '-C', 'link-arg=--build-id=none',
            $program.Source,
            '-o', $output
        )
        if (-not (Test-Path $output)) {
            throw "user image missing after build: $output"
        }
    }

    Write-Host ''
    Write-Host "User images: $outDir"
    Write-Host 'Userspace build complete.'
}
finally {
    Pop-Location
}
