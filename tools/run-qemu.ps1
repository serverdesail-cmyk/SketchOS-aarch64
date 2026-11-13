# Boot SketchOS under QEMU with EDK2 AArch64 firmware.
#
# Usage:
#   .\tools\run-qemu.ps1                  # build (if needed) and run with stdio
#   .\tools\run-qemu.ps1 -NoBuild         # run the current ESP
#   .\tools\run-qemu.ps1 -Serial build/serial.log   # log instead of stdio
#
# The VM exits by itself when the QEMU path reaches PSCI SYSTEM_OFF.
#
# The default accelerator is TCG. WHPX is accepted for diagnostics but cannot
# boot the bundled EDK2 AArch64 firmware or the CNTP-based kernel on this host.

param(
    [switch]$NoBuild,
    [string]$Esp = '',
    [string]$Serial = '',
    [int]$MemoryMiB = 2048,
    [int]$Cpus = 4,
    [ValidateSet('tcg', 'whpx')]
    [string]$Accel = 'tcg',
    [int]$TbSizeMiB = 256
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

if (-not $NoBuild) {
    & (Join-Path $PSScriptRoot 'build.ps1')
}

if (-not $Esp) { $Esp = Join-Path $root 'build/esp' }
if (-not (Test-Path $Esp)) { throw "ESP directory missing: $Esp" }

$qemu = 'C:\Program Files\qemu\qemu-system-aarch64.exe'
$firmware = 'C:\Program Files\qemu\share\edk2-aarch64-code.fd'
$varsTemplate = 'C:\Program Files\qemu\share\edk2-arm-vars.fd'
$vars = Join-Path $root 'build/edk2-vars.fd'

if (-not (Test-Path $qemu)) { throw "QEMU not found: $qemu" }
if (-not (Test-Path $firmware)) { throw "EDK2 firmware not found: $firmware" }
if (-not (Test-Path $varsTemplate)) { throw "EDK2 vars template not found: $varsTemplate" }
if (-not (Test-Path $vars)) { Copy-Item $varsTemplate $vars }

if ($Accel -eq 'whpx') {
    $cpu = 'host'
    $accelArgs = @('-accel', 'whpx')
} else {
    $cpu = 'max'
    $accelArgs = @('-accel', "tcg,thread=multi,tb-size=$TbSizeMiB")
}

$memId = 'sketchos-ram'
$arguments = @(
    '-machine', "virt,gic-version=3,memory-backend=$memId",
    '-cpu', $cpu,
    '-smp', "$Cpus",
    '-m', "$MemoryMiB",
    '-object', "memory-backend-ram,id=$memId,size=${MemoryMiB}M,prealloc=on",
    '-overcommit', 'mem-lock=off'
) + $accelArgs + @(
    '-drive', "if=pflash,format=raw,readonly=on,file=$firmware",
    '-drive', "if=pflash,format=raw,file=$vars",
    '-drive', "if=virtio,format=raw,file=fat:rw:$Esp",
    '-nic', 'none',
    '-display', 'none',
    '-monitor', 'none',
    '-no-reboot'
)

if ($Serial) {
    $serialPath = if ([System.IO.Path]::IsPathRooted($Serial)) { $Serial } else { Join-Path $root $Serial }
    $serialDir = Split-Path -Parent $serialPath
    if ($serialDir -and -not (Test-Path $serialDir)) {
        New-Item -ItemType Directory -Force -Path $serialDir | Out-Null
    }
    $arguments += @('-serial', "file:$serialPath")
    Write-Host "Serial log: $serialPath"
} else {
    $arguments += @('-serial', 'stdio')
}

Write-Host "QEMU: $qemu"
& $qemu @arguments
exit $LASTEXITCODE
