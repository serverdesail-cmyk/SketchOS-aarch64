# Build a bootable FAT32 EFI System Partition disk image from build/esp.
#
# The image is an ordinary file: this script never touches a physical disk,
# partition, bootloader or firmware setting. It writes an MBR with one type-0xEF
# partition and a FAT32 filesystem inside that partition, then re-opens the
# image and verifies the files by walking the FAT chains.
#
# Usage:
#   .\tools\make-esp-image.ps1
#   .\tools\make-esp-image.ps1 -Output .\build\custom.img
#
# The default output is build/sketchos-esp.img. A copy is also placed in
# images/sketchos-esp.img by tools/build.ps1.

param(
    [string]$Source = '',
    [string]$Output = ''
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $Source) { $Source = Join-Path $root 'build/esp' }
if (-not $Output) { $Output = Join-Path $root 'build/sketchos-esp.img' }

$loader = Join-Path $Source 'EFI/BOOT/BOOTAA64.EFI'
$kernel = Join-Path $Source 'EFI/SketchOS/kernel.elf'
if (-not (Test-Path $loader)) { throw "loader missing: $loader (run tools\build.ps1 first)" }
if (-not (Test-Path $kernel)) { throw "kernel missing: $kernel (run tools\build.ps1 first)" }

$bytesPerSector = 512
$partStartLba = 2048          # 1 MiB alignment, the usual ESP convention
$partSectors = 131072         # 64 MiB partition
$reservedSectors = 32
$numFats = 2
$secPerCluster = 1            # 512-byte clusters keep every file contiguous here
$rootCluster = 2

# Solve the FAT32 geometry. FAT size depends on the cluster count, which in
# turn depends on the FAT size; the fixed point below is the standard approach.
$fatSectors = 1024
while ($true) {
    $dataSectors = $partSectors - $reservedSectors - ($numFats * $fatSectors)
    $clusters = [math]::Floor($dataSectors / $secPerCluster)
    $needed = [math]::Ceiling((($clusters + 2) * 4) / $bytesPerSector)
    if ($fatSectors -ge $needed) { break }
    $fatSectors++
}
$totalClusters = [int]([math]::Floor(($partSectors - $reservedSectors - ($numFats * $fatSectors)) / $secPerCluster))
$dataStartSector = $reservedSectors + ($numFats * $fatSectors)   # partition-relative

function Set-U16([byte[]]$buffer, [int]$offset, [uint32]$value) {
    $buffer[$offset] = [byte]($value -band 0xFF)
    $buffer[$offset + 1] = [byte](($value -shr 8) -band 0xFF)
}

function Set-U32([byte[]]$buffer, [int]$offset, [uint32]$value) {
    $buffer[$offset] = [byte]($value -band 0xFF)
    $buffer[$offset + 1] = [byte](($value -shr 8) -band 0xFF)
    $buffer[$offset + 2] = [byte](($value -shr 16) -band 0xFF)
    $buffer[$offset + 3] = [byte](($value -shr 24) -band 0xFF)
}

function Get-U16([byte[]]$buffer, [int]$offset) {
    return [uint32]$buffer[$offset] -bor ([uint32]$buffer[$offset + 1] -shl 8)
}

function Get-U32([byte[]]$buffer, [int]$offset) {
    return [uint32]$buffer[$offset] -bor ([uint32]$buffer[$offset + 1] -shl 8) -bor
        ([uint32]$buffer[$offset + 2] -shl 16) -bor ([uint32]$buffer[$offset + 3] -shl 24)
}

# One 32-byte short (8.3) directory entry. All names used here fit, so no LFN
# entries are needed. The case-insensitive UEFI FAT driver resolves them.
function New-DirEntry([string]$name11, [byte]$attr, [uint32]$cluster, [uint32]$size) {
    if ($name11.Length -ne 11) { throw "short name must be 11 characters: '$name11'" }
    $entry = New-Object byte[] 32
    $nameBytes = [Text.Encoding]::ASCII.GetBytes($name11)
    [Array]::Copy($nameBytes, 0, $entry, 0, 11)
    $entry[11] = $attr
    # Fixed timestamp: 2026-11-12 12:00:00. Determinism makes the image hashable.
    $date = [uint32](((2026 - 1980) -shl 9) -bor (11 -shl 5) -bor 12)
    $time = [uint32]((12 -shl 11) -bor (0 -shl 5) -bor 0)
    Set-U16 $entry 14 $time
    Set-U16 $entry 16 $date
    Set-U16 $entry 18 $date
    Set-U16 $entry 22 $time
    Set-U16 $entry 24 $date
    Set-U16 $entry 20 ([uint32](($cluster -shr 16) -band 0xFFFF))
    Set-U16 $entry 26 ([uint32]($cluster -band 0xFFFF))
    Set-U32 $entry 28 $size
    return $entry
}

function New-Directory([object[]]$entries) {
    $data = New-Object byte[] ($secPerCluster * $bytesPerSector)
    $offset = 0
    foreach ($entry in $entries) {
        [Array]::Copy($entry, 0, $data, $offset, 32)
        $offset += 32
        if ($offset + 32 -gt $data.Length) { throw 'directory does not fit in one cluster' }
    }
    return $data
}

# --- Allocate and write ----------------------------------------------------

$loaderBytes = [IO.File]::ReadAllBytes($loader)
$kernelBytes = [IO.File]::ReadAllBytes($kernel)
$clusterBytes = $secPerCluster * $bytesPerSector
$loaderClusters = [int][math]::Ceiling($loaderBytes.Length / $clusterBytes)
$kernelClusters = [int][math]::Ceiling($kernelBytes.Length / $clusterBytes)

$nextCluster = 2
$rootDirCluster = $nextCluster; $nextCluster += 1
$efiCluster = $nextCluster; $nextCluster += 1
$bootCluster = $nextCluster; $nextCluster += 1
$osCluster = $nextCluster; $nextCluster += 1
$loaderCluster = $nextCluster; $nextCluster += $loaderClusters
$kernelCluster = $nextCluster; $nextCluster += $kernelClusters
$usedClusters = $nextCluster - 2
if ($usedClusters -gt $totalClusters) { throw "files do not fit: $usedClusters clusters needed, $totalClusters available" }

$fat = New-Object 'uint32[]' ($totalClusters + 2)
$fat[0] = [uint32]0x0FFFFFF8
$fat[1] = [uint32]0x0FFFFFFF
function Set-Chain([int]$first, [int]$count) {
    if ($count -lt 1) { throw 'empty chain' }
    for ($i = 0; $i -lt $count; $i++) {
        if ($i -eq $count - 1) {
            $fat[$first + $i] = [uint32]0x0FFFFFFF
        } else {
            $fat[$first + $i] = [uint32]($first + $i + 1)
        }
    }
}
Set-Chain $rootDirCluster 1
Set-Chain $efiCluster 1
Set-Chain $bootCluster 1
Set-Chain $osCluster 1
Set-Chain $loaderCluster $loaderClusters
Set-Chain $kernelCluster $kernelClusters

$dot = New-DirEntry '.          ' 0x10 $rootDirCluster 0
$dotdotRoot = New-DirEntry '..         ' 0x10 0 0
$dotEfi = New-DirEntry '.          ' 0x10 $efiCluster 0
$dotdotEfi = New-DirEntry '..         ' 0x10 $rootDirCluster 0
$dotBoot = New-DirEntry '.          ' 0x10 $bootCluster 0
$dotdotBoot = New-DirEntry '..         ' 0x10 $efiCluster 0
$dotOs = New-DirEntry '.          ' 0x10 $osCluster 0
$dotdotOs = New-DirEntry '..         ' 0x10 $efiCluster 0

$rootData = New-Directory @(
    (New-DirEntry 'SKETCHOS   ' 0x08 0 0),
    (New-DirEntry 'EFI        ' 0x10 $efiCluster 0)
)
$efiData = New-Directory @(
    $dotEfi,
    $dotdotEfi,
    (New-DirEntry 'BOOT       ' 0x10 $bootCluster 0),
    (New-DirEntry 'SKETCHOS   ' 0x10 $osCluster 0)
)
$bootData = New-Directory @(
    $dotBoot,
    $dotdotBoot,
    (New-DirEntry 'BOOTAA64EFI' 0x20 $loaderCluster ([uint32]$loaderBytes.Length))
)
$osData = New-Directory @(
    $dotOs,
    $dotdotOs,
    (New-DirEntry 'KERNEL  ELF' 0x20 $kernelCluster ([uint32]$kernelBytes.Length))
)

$fs = [IO.File]::Open($Output, [IO.FileMode]::Create, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
try {
    $fs.SetLength([long]($partStartLba + $partSectors) * $bytesPerSector)

    # MBR with one EFI System Partition entry.
    $mbr = New-Object byte[] 512
    $mbr[0x1BE] = 0x00
    $mbr[0x1BF] = 0xFE; $mbr[0x1C0] = 0xFF; $mbr[0x1C1] = 0xFF
    $mbr[0x1C2] = 0xEF
    $mbr[0x1C3] = 0xFE; $mbr[0x1C4] = 0xFF; $mbr[0x1C5] = 0xFF
    Set-U32 $mbr 0x1C6 ([uint32]$partStartLba)
    Set-U32 $mbr 0x1CA ([uint32]$partSectors)
    $mbr[0x1FE] = 0x55; $mbr[0x1FF] = 0xAA
    $fs.Seek(0, 'Begin') | Out-Null
    $fs.Write($mbr, 0, 512)

    # FAT32 boot sector.
    $boot = New-Object byte[] 512
    $boot[0] = 0xEB; $boot[1] = 0x58; $boot[2] = 0x90
    [Array]::Copy([Text.Encoding]::ASCII.GetBytes('MSWIN4.1'), 0, $boot, 3, 8)
    Set-U16 $boot 0x0B ([uint32]$bytesPerSector)
    $boot[0x0D] = [byte]$secPerCluster
    Set-U16 $boot 0x0E ([uint32]$reservedSectors)
    $boot[0x10] = [byte]$numFats
    Set-U16 $boot 0x11 0
    Set-U16 $boot 0x13 0
    $boot[0x15] = 0xF8
    Set-U16 $boot 0x16 0
    Set-U16 $boot 0x18 63
    Set-U16 $boot 0x1A 255
    Set-U32 $boot 0x1C ([uint32]$partStartLba)
    Set-U32 $boot 0x20 ([uint32]$partSectors)
    Set-U32 $boot 0x24 ([uint32]$fatSectors)
    Set-U16 $boot 0x28 0
    Set-U16 $boot 0x2A 0
    Set-U32 $boot 0x2C ([uint32]$rootCluster)
    Set-U16 $boot 0x30 1
    Set-U16 $boot 0x32 6
    $boot[0x40] = 0x80
    $boot[0x42] = 0x29
    Set-U32 $boot 0x43 ([uint32]0x5C4B534F)
    [Array]::Copy([Text.Encoding]::ASCII.GetBytes('SKETCHOS   '), 0, $boot, 0x47, 11)
    [Array]::Copy([Text.Encoding]::ASCII.GetBytes('FAT32   '), 0, $boot, 0x52, 8)
    $boot[0x1FE] = 0x55; $boot[0x1FF] = 0xAA
    $partitionOffset = [long]$partStartLba * $bytesPerSector
    $fs.Seek($partitionOffset, 'Begin') | Out-Null
    $fs.Write($boot, 0, 512)

    # FSInfo, with an accurate free-cluster count.
    $fsinfo = New-Object byte[] 512
    Set-U32 $fsinfo 0x00 ([uint32]0x41615252)
    Set-U32 $fsinfo 0x1E4 ([uint32]0x61417272)
    $freeClusters = $totalClusters - $usedClusters
    Set-U32 $fsinfo 0x1E8 ([uint32]$freeClusters)
    Set-U32 $fsinfo 0x1EC ([uint32]$nextCluster)
    Set-U32 $fsinfo 0x1FC ([Convert]::ToUInt32('AA550000', 16))
    $fs.Seek($partitionOffset + (1 * $bytesPerSector), 'Begin') | Out-Null
    $fs.Write($fsinfo, 0, 512)

    # Backup boot sector and FSInfo, as required for FAT32.
    $fs.Seek($partitionOffset + (6 * $bytesPerSector), 'Begin') | Out-Null
    $fs.Write($boot, 0, 512)
    $fs.Seek($partitionOffset + (7 * $bytesPerSector), 'Begin') | Out-Null
    $fs.Write($fsinfo, 0, 512)

    # Both FAT copies.
    $fatBytes = New-Object byte[] (($totalClusters + 2) * 4)
    [System.Buffer]::BlockCopy($fat, 0, $fatBytes, 0, $fatBytes.Length)
    for ($copy = 0; $copy -lt $numFats; $copy++) {
        $offset = $partitionOffset + (([long]$reservedSectors + ([long]$copy * $fatSectors)) * $bytesPerSector)
        $fs.Seek($offset, 'Begin') | Out-Null
        $fs.Write($fatBytes, 0, $fatBytes.Length)
    }

    function Write-ClusterData([byte[]]$data, [int]$first, [int]$count) {
        $expected = $count * $clusterBytes
        if ($data.Length -gt $expected) { throw 'cluster chain too small for data' }
        $padded = New-Object byte[] $expected
        [Array]::Copy($data, 0, $padded, 0, $data.Length)
        $offset = $partitionOffset + (([long]$dataStartSector + ([long]($first - 2) * $secPerCluster)) * $bytesPerSector)
        $fs.Seek($offset, 'Begin') | Out-Null
        $fs.Write($padded, 0, $padded.Length)
    }

    Write-ClusterData $rootData $rootDirCluster 1
    Write-ClusterData $efiData $efiCluster 1
    Write-ClusterData $bootData $bootCluster 1
    Write-ClusterData $osData $osCluster 1
    Write-ClusterData $loaderBytes $loaderCluster $loaderClusters
    Write-ClusterData $kernelBytes $kernelCluster $kernelClusters
    $fs.Flush()
}
finally {
    $fs.Dispose()
}

# --- Verify by re-reading the image ---------------------------------------

$image = [IO.File]::ReadAllBytes($Output)
function Read-At([int]$offset, [int]$count) {
    $buffer = New-Object byte[] $count
    [Array]::Copy($image, $offset, $buffer, 0, $count)
    return $buffer
}

$mbrCheck = Read-At 0 512
if ((Get-U16 $mbrCheck 0x1FE) -ne 0xAA55) { throw 'MBR signature is missing' }
if ($mbrCheck[0x1C2] -ne 0xEF) { throw 'partition type is not 0xEF (EFI System Partition)' }
$checkStart = Get-U32 $mbrCheck 0x1C6
$checkSectors = Get-U32 $mbrCheck 0x1CA
if ($checkStart -ne $partStartLba -or $checkSectors -ne $partSectors) { throw 'partition entry does not match the planned layout' }

$checkBoot = Read-At ([int]($checkStart * $bytesPerSector)) 512
if ((Get-U16 $checkBoot 0x1FE) -ne 0xAA55) { throw 'FAT32 boot sector signature is missing' }
$checkFatSectors = Get-U32 $checkBoot 0x24
$checkRoot = Get-U32 $checkBoot 0x2C
$checkDataStart = $reservedSectors + ($numFats * $checkFatSectors)
if ((Get-U32 $checkBoot 0x20) -ne $partSectors) { throw 'FAT32 total-sector field is wrong' }
if ($checkRoot -ne 2) { throw 'FAT32 root cluster is wrong' }

$checkFat = New-Object 'uint32[]' ($totalClusters + 2)
$fatOffset = [int](($checkStart + $reservedSectors) * $bytesPerSector)
for ($i = 0; $i -lt $checkFat.Length; $i++) {
    $checkFat[$i] = Get-U32 $image ($fatOffset + ($i * 4))
}

function Read-Chain([int]$first) {
    if ($first -lt 2 -or $first -gt ($totalClusters + 1)) { throw "cluster out of range: $first" }
    $segments = New-Object System.Collections.Generic.List[byte[]]
    $cluster = $first
    $guard = 0
    while ($true) {
        $offset = [int](($checkStart + $checkDataStart + (($cluster - 2) * $secPerCluster)) * $bytesPerSector)
        $segments.Add((Read-At $offset $clusterBytes))
        $next = $checkFat[$cluster] -band 0x0FFFFFFF
        if ($next -eq 0x0FFFFFFF) { break }
        if ($next -lt 2 -or $next -gt ($totalClusters + 1)) { throw "bad FAT link $next from cluster $cluster" }
        $cluster = $next
        $guard++
        if ($guard -gt $totalClusters) { throw 'FAT chain loop' }
    }
    $result = New-Object byte[] ($segments.Count * $clusterBytes)
    $offset = 0
    foreach ($segment in $segments) {
        [Array]::Copy($segment, 0, $result, $offset, $segment.Length)
        $offset += $segment.Length
    }
    return $result
}

function Find-Entry([byte[]]$directory, [string]$name11) {
    for ($offset = 0; $offset + 32 -le $directory.Length; $offset += 32) {
        $attr = $directory[$offset + 11]
        if ($attr -eq 0x0F) { continue }
        if ($directory[$offset] -eq 0x00) { break }
        if ($directory[$offset] -eq 0xE5) { continue }
        $name = [Text.Encoding]::ASCII.GetString($directory, $offset, 11)
        if ($name -ne $name11) { continue }
        return @{
            Cluster = ([uint32](Get-U16 $directory ($offset + 26))) -bor
                ([uint32](Get-U16 $directory ($offset + 20)) -shl 16)
            Size = Get-U32 $directory ($offset + 28)
            Attr = $attr
        }
    }
    throw "directory entry not found: '$name11'"
}

function Get-PathBytes([string]$path) {
    $segments = $path.Split('/')
    $cluster = $checkRoot
    $directory = $null
    for ($i = 0; $i -lt $segments.Count; $i++) {
        $directory = Read-Chain $cluster
        $entry = Find-Entry $directory $segments[$i]
        if ($i -eq $segments.Count - 1) {
            $data = Read-Chain $entry.Cluster
            return $data[0..([int]$entry.Size - 1)]
        }
        if (($entry.Attr -band 0x10) -eq 0) { throw "not a directory: $($segments[$i])" }
        $cluster = $entry.Cluster
    }
    throw "empty path"
}

function Get-Sha256([byte[]]$data) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($sha.ComputeHash($data))).Replace('-', '').ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
    }
}

$checkLoader = Get-PathBytes 'EFI        /BOOT       /BOOTAA64EFI'
$checkKernel = Get-PathBytes 'EFI        /SKETCHOS   /KERNEL  ELF'
$loaderHash = Get-Sha256 $loaderBytes
$kernelHash = Get-Sha256 $kernelBytes
if ((Get-Sha256 $checkLoader) -ne $loaderHash) { throw 'BOOTAA64.EFI read back differs from the source file' }
if ((Get-Sha256 $checkKernel) -ne $kernelHash) { throw 'kernel.elf read back differs from the source file' }

$imageHash = Get-Sha256 $image
Write-Host ''
Write-Host "ESP image: $Output"
Write-Host "  size       $($image.Length) bytes ($([math]::Round($image.Length / 1MB, 2)) MiB)"
Write-Host "  partition  type 0xEF, LBA $checkStart, $checkSectors sectors"
Write-Host "  FAT32      $totalClusters clusters of $clusterBytes bytes, $fatSectors FAT sectors"
Write-Host "  loader     $($checkLoader.Length) bytes, sha256 $loaderHash"
Write-Host "  kernel     $($checkKernel.Length) bytes, sha256 $kernelHash"
Write-Host "  image      sha256 $imageHash"
Write-Host '  verified: MBR, FAT32 geometry, FAT chains and both files'
