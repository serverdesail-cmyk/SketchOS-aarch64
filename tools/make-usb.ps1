# Copy the SketchOS UEFI boot files to a removable USB medium.
#
# This script never installs to an internal disk. It requires an explicit drive
# letter, refuses the system drive and fixed disks, and only formats when both
# -Format and -ConfirmFormat are supplied.
#
# Usage:
#   .\tools\make-usb.ps1 -Drive E:
#   .\tools\make-usb.ps1 -Drive E: -Format -ConfirmFormat
#
# The medium must be FAT32. Without -Format, an existing FAT32 volume is
# required and only the SketchOS files are replaced.

param(
    [Parameter(Mandatory = $true)]
    [string]$Drive,

    [switch]$Format,

    [switch]$ConfirmFormat
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

function Fail([string]$Message) {
    Write-Host "FAIL: $Message"
    exit 1
}

# --- Validate the target -------------------------------------------------

$letter = $Drive.Trim().TrimEnd('\').TrimEnd(':').ToUpperInvariant()
if ($letter -notmatch '^[A-Z]$') {
    Fail "drive must be a single letter such as E: (got '$Drive')"
}

$systemLetter = $env:SystemDrive.TrimEnd(':').ToUpperInvariant()
if ($letter -eq $systemLetter) {
    Fail "refusing to write to the system drive (${letter}:)"
}

$volume = Get-Volume -DriveLetter $letter -ErrorAction SilentlyContinue
if (-not $volume) {
    Fail "no volume found for $($letter):"
}
if ($volume.DriveType -ne 'Removable') {
    Fail "refusing to write to a non-removable volume ($($letter): is $($volume.DriveType))"
}
if ($volume.FileSystem -and $volume.FileSystem -ne 'FAT32' -and -not $Format) {
    Fail "volume is $($volume.FileSystem); use a FAT32 medium or pass -Format -ConfirmFormat"
}

if ($Format) {
    if (-not $ConfirmFormat) {
        Fail 'formatting requires both -Format and -ConfirmFormat; nothing was changed'
    }
    Write-Host "Formatting $($letter): as FAT32 (all data on this volume will be lost)..."
    Format-Volume -DriveLetter $letter -FileSystem FAT32 -NewFileSystemLabel 'SKETCHOS' -Confirm:$false | Out-Null
}

# --- Validate the build outputs -----------------------------------------

$loader = Join-Path $root 'images/BOOTAA64.EFI'
$kernel = Join-Path $root 'images/sketchos-kernel.elf'
if (-not (Test-Path $loader)) { Fail "loader missing: $loader (run tools\build.ps1 first)" }
if (-not (Test-Path $kernel)) { Fail "kernel missing: $kernel (run tools\build.ps1 first)" }

# --- Copy the ESP layout -------------------------------------------------

$target = "$($letter):\"
$bootDir = Join-Path $target 'EFI/BOOT'
$osDir = Join-Path $target 'EFI/SketchOS'

New-Item -ItemType Directory -Force -Path $bootDir, $osDir | Out-Null
Copy-Item -Force $loader (Join-Path $bootDir 'BOOTAA64.EFI')
Copy-Item -Force $kernel (Join-Path $osDir 'kernel.elf')

# --- Verify --------------------------------------------------------------

$copiedLoader = Join-Path $bootDir 'BOOTAA64.EFI'
$copiedKernel = Join-Path $osDir 'kernel.elf'
if (-not (Test-Path $copiedLoader)) { Fail "copy failed: $copiedLoader" }
if (-not (Test-Path $copiedKernel)) { Fail "copy failed: $copiedKernel" }

Write-Host ''
Write-Host "USB medium: $($letter):"
Write-Host "  $copiedLoader"
Write-Host "  $copiedKernel"
Write-Host ''
Write-Host 'The medium is ready. Booting the physical laptop still requires'
Write-Host 'explicit confirmation and a Secure Boot decision. See docs/real-hardware.md.'
