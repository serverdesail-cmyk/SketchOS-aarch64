# Build the SketchOS kernel and UEFI loader, then assemble a bootable ESP.
#
# The 0.0.5 user images are built first: the kernel embeds them with
# include_bytes!, so they must exist before Cargo compiles the kernel.
#
# Usage:
#   .\tools\build.ps1            # release build
#   .\tools\build.ps1 -Debug     # debug build
#
# Output:
#   build/esp/EFI/BOOT/BOOTAA64.EFI
#   build/esp/EFI/SketchOS/kernel.elf
#   images/BOOTAA64.EFI
#   images/sketchos-kernel.elf

param(
    [switch]$Debug,
    [switch]$Shell
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

function Invoke-Cargo {
    param([string[]]$CargoArgs)

    # Cargo writes progress and warnings to stderr. Windows PowerShell turns
    # those into error records, so merge them and only trust the exit code.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & cargo @CargoArgs 2>&1 | ForEach-Object { Write-Host $_ }
    $code = $LASTEXITCODE
    $ErrorActionPreference = $previous
    if ($code -ne 0) {
        throw "cargo failed ($code): cargo $($CargoArgs -join ' ')"
    }
}

try {
    $release = -not $Debug
    $profileArgs = if ($release) { @('--release') } else { @() }
    $outDir = if ($release) { 'release' } else { 'debug' }

    Write-Host '==> Building user images (aarch64-unknown-none)'
    & (Join-Path $PSScriptRoot 'build-userspace.ps1')
    if ($LASTEXITCODE -ne 0) {
        throw "userspace build failed ($LASTEXITCODE)"
    }

    Write-Host '==> Building kernel (aarch64-unknown-none)'
    $featureArgs = if ($Shell) { @('--features', 'shell') } else { @() }
Invoke-Cargo (@('build') + $profileArgs + $featureArgs + @('--target', 'aarch64-unknown-none', '-p', 'sketchos-kernel'))

    Write-Host '==> Building UEFI loader (aarch64-unknown-uefi)'
    Invoke-Cargo (@('build') + $profileArgs + @('--target', 'aarch64-unknown-uefi', '-p', 'sketchos-uefi'))

    $kernel = Join-Path $root "target/aarch64-unknown-none/$outDir/sketchos-kernel"
    $loader = Join-Path $root "target/aarch64-unknown-uefi/$outDir/sketchos-uefi.efi"

    if (-not (Test-Path $kernel)) { throw "kernel artifact missing: $kernel" }
    if (-not (Test-Path $loader)) { throw "loader artifact missing: $loader" }

    $esp = Join-Path $root 'build/esp'
    if (Test-Path $esp) { Remove-Item -Recurse -Force $esp }
    $bootDir = Join-Path $esp 'EFI/BOOT'
    $osDir = Join-Path $esp 'EFI/SketchOS'
    New-Item -ItemType Directory -Force -Path $bootDir, $osDir | Out-Null

    Copy-Item -Force $loader (Join-Path $bootDir 'BOOTAA64.EFI')
    Copy-Item -Force $kernel (Join-Path $osDir 'kernel.elf')

    $images = Join-Path $root 'images'
    New-Item -ItemType Directory -Force -Path $images | Out-Null
    Copy-Item -Force $loader (Join-Path $images 'BOOTAA64.EFI')
    Copy-Item -Force $kernel (Join-Path $images 'sketchos-kernel.elf')

    Write-Host '==> Building bootable FAT32 ESP image'
    & (Join-Path $PSScriptRoot 'make-esp-image.ps1')
    if ($LASTEXITCODE -ne 0) {
        throw "ESP image build failed ($LASTEXITCODE)"
    }
    $image = Join-Path $root 'build/sketchos-esp.img'
    Copy-Item -Force $image (Join-Path $images 'sketchos-esp.img')

    Write-Host ''
    Write-Host "Kernel: $kernel"
    Write-Host "Loader: $loader"
    Write-Host "ESP:    $esp"
    Write-Host "Image:  $image"
    Write-Host 'Build complete.'
}
finally {
    Pop-Location
}
