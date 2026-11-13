param(
    [string]$EspImage = '',
    [string]$Output = '',
    [string]$EspDirectory = '',
    [string]$Xorriso = 'xorriso'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $EspImage) { $EspImage = Join-Path $root 'build/sketchos-esp.img' }
if (-not $Output) { $Output = Join-Path $root 'images/SketchOS-AArch64.iso' }
if (-not $EspDirectory) { $EspDirectory = Join-Path $root 'build/esp' }
$tool = Get-Command $Xorriso -ErrorAction SilentlyContinue
if (-not $tool) { throw 'xorriso was not found in PATH.' }
if (-not (Test-Path $EspImage)) { throw "ESP image missing: $EspImage (run tools\build.ps1 first)." }
if (-not (Test-Path (Join-Path $EspDirectory 'EFI/BOOT/BOOTAA64.EFI'))) { throw "EFI tree missing: $EspDirectory (run tools\build.ps1 first)." }
$outputDirectory = Split-Path -Parent $Output
New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
$stage = Join-Path $root 'build/iso-root'
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item -Force $EspImage (Join-Path $stage 'sketchos-esp.img')
# Also expose the standard EFI file tree at ISO root. This helps imaging tools
# that copy the ISO filesystem to a USB device rather than using El Torito.
Copy-Item -Recurse -Force (Join-Path $EspDirectory 'EFI') $stage
@'
SketchOS AArch64
================
UEFI boot image for AArch64 systems.
The EFI System Partition is sketchos-esp.img. It contains:
  EFI/BOOT/BOOTAA64.EFI
  EFI/SketchOS/kernel.elf
This ISO is a boot medium, not an installer. It does not partition or modify
an internal drive. Boot it from removable media or QEMU.
'@ | Set-Content -Encoding ASCII (Join-Path $stage 'README.txt')
if (Test-Path $Output) { Remove-Item -Force $Output }
Write-Host "Creating UEFI El Torito ISO: $Output"
& $tool.Source -as mkisofs -iso-level 3 -full-iso9660-filenames -volid SKETCHOS -e sketchos-esp.img -no-emul-boot -o $Output $stage
if ($LASTEXITCODE -ne 0) { throw "xorriso ISO creation failed ($LASTEXITCODE)." }
if (-not (Test-Path $Output)) { throw "xorriso did not create $Output" }
& $tool.Source -indev $Output -report_el_torito plain
if ($LASTEXITCODE -ne 0) {
    Remove-Item -Force $Output
    throw 'ISO verification failed: xorriso could not read its El Torito catalog.'
}
$iso = Get-Item $Output
$hash = (Get-FileHash -Algorithm SHA256 $Output).Hash.ToLowerInvariant()
Write-Host "ISO: $($iso.FullName)"
Write-Host "Size: $([math]::Round($iso.Length / 1MB, 2)) MiB"
Write-Host "SHA256: $hash"
Write-Host 'Verified: ISO readable and EFI El Torito boot entry present.'
