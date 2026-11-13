# Automated QEMU smoke test.
#
# Boots the current ESP, captures the PL011 log, and requires the kernel
# success marker. QEMU is expected to power off through PSCI; if it does not,
# the test fails after the timeout instead of hanging forever.
#
# Usage:
#   .\tools\smoke-test.ps1
#   .\tools\smoke-test.ps1 -TimeoutSeconds 180 -KeepLog
#   .\tools\smoke-test.ps1 -Accel whpx -TimeoutSeconds 60
#
# Host tuning (on by default, disable for an A/B comparison):
#   * one explicit, preallocated RAM backend instead of a lazily faulted
#     anonymous region, so timed runs are not interrupted by host page faults;
#   * 0.5 ms Windows timer resolution while QEMU runs, which keeps the
#     one-shot guest timer's host deadline from drifting by a full tick;
#   * High process priority so a busy desktop does not preempt the vCPUs.
#
# -Accel whpx is diagnostic only on this host: QEMU 11.1.0's WHPX backend does
# not boot the bundled EDK2 AArch64 firmware and does not expose the EL1
# physical timer (CNTP_CVAL_EL0) the kernel programs, so a run gets a clear
# failure instead of a fake pass.

param(
    [int]$TimeoutSeconds = 120,
    [int]$MemoryMiB = 2048,
    [int]$Cpus = 8,
    [ValidateSet('tcg', 'whpx')]
    [string]$Accel = 'tcg',
    [int]$TbSizeMiB = 256,
    [switch]$NoPrealloc,
    [switch]$NoTuning,
    [switch]$NoBuild,
    [switch]$KeepLog,
    [switch]$Framebuffer,
    [switch]$OccupyLegacy,
    [switch]$Virtualization,
    [switch]$ExpectEl2,
    [switch]$Debug,
    [switch]$TwoCluster,
    [string]$EspImage = ''
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$log = Join-Path $root 'build/serial.log'

if (-not $NoBuild) {
    if ($Debug) {
        & (Join-Path $PSScriptRoot 'build.ps1') -SmokeTest -Debug
    } else {
        & (Join-Path $PSScriptRoot 'build.ps1') -SmokeTest
    }
}

if (Test-Path $log) { Remove-Item -Force $log }

$qemu = 'C:\Program Files\qemu\qemu-system-aarch64.exe'
$firmware = 'C:\Program Files\qemu\share\edk2-aarch64-code.fd'
$varsTemplate = 'C:\Program Files\qemu\share\edk2-arm-vars.fd'
$vars = Join-Path $root 'build/edk2-vars.fd'
$esp = Join-Path $root 'build/esp'

if (-not (Test-Path $vars)) { Copy-Item $varsTemplate $vars }

# `-EspImage` boots the standalone FAT32 ESP image instead of build/esp, which
# proves the artifact produced by tools/make-esp-image.ps1 is bootable rather
# than only structurally valid. The relocation marker is a file inside the ESP,
# so the two modes cannot be combined without rebuilding the image.
$bootDrive = "if=virtio,format=raw,file=fat:rw:$esp"
if ($EspImage) {
    if ($OccupyLegacy) { throw '-EspImage cannot be combined with -OccupyLegacy.' }
    $imagePath = if ([IO.Path]::IsPathRooted($EspImage)) { $EspImage } else { Join-Path $root $EspImage }
    if (-not (Test-Path $imagePath)) { throw "ESP image missing: $imagePath" }
    $bootDrive = "if=virtio,format=raw,file=$imagePath"
    Write-Host "Booting the standalone ESP image: $imagePath"
}

# The relocation test asks the loader to occupy the old fixed link address.
# Always remove the marker first so a previous run cannot change this one.
$legacyMarker = Join-Path $esp 'EFI/SketchOS/occupy-legacy'
if ($OccupyLegacy) {
    New-Item -ItemType File -Force -Path $legacyMarker | Out-Null
    Write-Host 'Relocation test marker: occupying 0x40200000 before kernel load.'
} else {
    Remove-Item -Force -ErrorAction SilentlyContinue $legacyMarker
}

$machine = 'virt,gic-version=3'
if ($Virtualization) { $machine = 'virt,gic-version=3,virtualization=on' }

# Guest RAM is one explicit backend. `prealloc=on` populates the whole mapping
# before the guest starts, which removes host page-fault latency from timed
# runs; `mem-lock=off` (the default, stated explicitly) leaves those pages
# reclaimable instead of pinning 2 GiB. On a unified-memory host the GPU and CPU
# share the same DRAM, so pinning guest RAM would starve the compositor rather
# than help the guest. One backend also means no balloon, no overcommit and no
# second anonymous mapping for the same bytes.
$memId = 'sketchos-ram'
$machine = "$machine,memory-backend=$memId"

# The default profile is eight vCPUs. `-TwoCluster` requests two clusters of
# four cores each; the kernel only claims two clusters when ACPI PPTT describes
# them, so a QEMU version that ignores the topology fails the assertion honestly.
$smp = "$Cpus"
if ($TwoCluster) {
    if ($Cpus % 2 -ne 0) { throw 'TwoCluster requires an even CPU count.' }
    $smp = "$Cpus,clusters=2,cores=$($Cpus / 2)"
}

if ($Accel -eq 'whpx') {
    # Only the real ARM CPU models are accepted by WHPX; `max` is a TCG model.
    $cpu = 'host'
    $accelArgs = @('-accel', 'whpx')
} else {
    $cpu = 'max'
    # thread=multi runs one host thread per vCPU (the default on SMP, stated
    # explicitly); a larger translation-block cache cuts re-translation cost on
    # the long scheduler demo without touching guest semantics.
    $accelArgs = @('-accel', "tcg,thread=multi,tb-size=$TbSizeMiB")
}

$prealloc = if ($NoPrealloc) { 'off' } else { 'on' }

$arguments = @('-machine', $machine) + $accelArgs + @(
    '-cpu', $cpu,
    '-smp', $smp,
    '-m', "$MemoryMiB",
    '-object', "memory-backend-ram,id=$memId,size=${MemoryMiB}M,prealloc=$prealloc",
    '-overcommit', 'mem-lock=off',
    '-drive', "if=pflash,format=raw,readonly=on,file=$firmware",
    '-drive', "if=pflash,format=raw,file=$vars",
    '-drive', $bootDrive,
    '-nic', 'none',
    '-monitor', 'none',
    '-no-reboot',
    '-serial', "file:$log"
)

if ($Framebuffer) {
    # ramfb is a simple linear framebuffer that the EDK2 firmware exposes as a
    # GOP. It works without a host display, so the test stays headless.
    $arguments = @('-device', 'ramfb') + $arguments + @('-display', 'none')
} else {
    $arguments = $arguments + @('-display', 'none')
}

Write-Host "Booting QEMU with -accel $Accel (timeout ${TimeoutSeconds}s)..."
if ($Accel -eq 'whpx') {
    Write-Host 'NOTE: WHPX cannot boot the bundled EDK2 AArch64 firmware and does not expose CNTP_CVAL_EL0; this run is expected to fail.'
}
# Start-Process joins an argument array with spaces and does not add quotes,
# so arguments containing spaces (the firmware paths) must be quoted here.
$argLine = ($arguments | ForEach-Object {
    if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
}) -join ' '
$qemuErr = Join-Path $root 'build/qemu.stderr.log'
Remove-Item -Force -ErrorAction SilentlyContinue $qemuErr

# Windows' default timer resolution is 15.6 ms (or a vendor power-plan value),
# which is far coarser than the 4 ms scheduler slices this test measures. A
# 0.5 ms resolution is the finest supported and is restored after the run.
$timerPrevious = $null
if (-not $NoTuning) {
    try {
        if (-not ('SketchOS.NativeTimer' -as [type])) {
            Add-Type -Namespace SketchOS -Name NativeTimer -MemberDefinition @'
[DllImport("ntdll.dll")] public static extern int NtQueryTimerResolution(out uint minimum, out uint maximum, out uint current);
[DllImport("ntdll.dll")] public static extern int NtSetTimerResolution(uint desired, bool set, out uint current);
'@
        }
        $min = 0; $max = 0; $current = 0
        if ([SketchOS.NativeTimer]::NtQueryTimerResolution([ref]$min, [ref]$max, [ref]$current) -eq 0) {
            $timerPrevious = $current
            $actual = 0
            if ([SketchOS.NativeTimer]::NtSetTimerResolution(5000, $true, [ref]$actual) -eq 0) {
                Write-Host ("Host timer resolution: 0.5 ms (was {0:N2} ms)." -f ($current / 10000.0))
            } else {
                $timerPrevious = $null
            }
        }
    } catch {
        Write-Host "Host timer resolution: not adjusted ($($_.Exception.Message))."
    }
}

$process = Start-Process -FilePath $qemu -ArgumentList $argLine -PassThru -WindowStyle Hidden -RedirectStandardError $qemuErr

if (-not $NoTuning) {
    try {
        $process.PriorityClass = [System.Diagnostics.ProcessPriorityClass]::High
        Write-Host 'QEMU process priority: High.'
    } catch {
        Write-Host "QEMU process priority: not raised ($($_.Exception.Message))."
    }
}

$finished = $process.WaitForExit($TimeoutSeconds * 1000)
if (-not $finished) {
    Write-Host 'QEMU did not power off; terminating it.'
    try { $process.Kill() } catch { }
    $process.WaitForExit()
}

if ($null -ne $timerPrevious) {
    try {
        $restored = 0
        [void][SketchOS.NativeTimer]::NtSetTimerResolution([uint32]$timerPrevious, $true, [ref]$restored)
    } catch { }
}

if (Test-Path $qemuErr) {
    $stderr = Get-Content $qemuErr -Raw
    if ($stderr -and $stderr.Trim()) {
        Write-Host ''
        Write-Host '===== QEMU stderr ====='
        Write-Host $stderr.Trim()
        Write-Host '===== end stderr ====='
    }
}

if (Test-Path $log) {
    Write-Host ''
    Write-Host '===== serial log ====='
    Get-Content $log | ForEach-Object { Write-Host $_ }
    Write-Host '===== end log ====='
} else {
    Write-Host 'No serial log was produced.'
}

$result = 0
if (-not $finished) {
    Write-Host 'FAIL: QEMU timed out instead of powering off.'
    $result = 1
} elseif (-not (Test-Path $log)) {
    Write-Host 'FAIL: no serial output.'
    $result = 1
} else {
    $text = Get-Content $log -Raw
    if ($text -notmatch 'SKETCHOS 0\.0\.7 KERNEL OK') {
        Write-Host 'FAIL: kernel success marker missing.'
        $result = 1
    }
    if ($text -notmatch 'SketchOS UEFI loader 0\.0\.7 \(AArch64\)') {
        Write-Host 'FAIL: loader banner is not 0.0.6.'
        $result = 1
    }
    if ($text -notmatch 'SketchOS kernel 0\.0\.7 \(AArch64\)') {
        Write-Host 'FAIL: kernel banner is not 0.0.6.'
        $result = 1
    }
    if ($text -notmatch 'MMU: higher half, TTBR1 0x[0-9a-f]+, TTBR0 boot 0x[0-9a-f]+, \d+ tables of 2048, physmap \d+ GiB') {
        Write-Host 'FAIL: kernel did not install its own higher-half map.'
        $result = 1
    }
    if ($text -notmatch 'MMU: W\^X audit clean') {
        Write-Host 'FAIL: the final map has a writable-executable page or was not audited.'
        $result = 1
    }
    if ($text -notmatch 'MMU: address-space self-test passed') {
        Write-Host 'FAIL: the ASID/TTBR0 address-space self-test did not pass.'
        $result = 1
    }
    if ($text -notmatch 'MMU: fault self-test passed') {
        Write-Host 'FAIL: the recoverable-fault self-test did not pass.'
        $result = 1
    }
    if ($text -notmatch 'MMU: TTBR0 walks disabled; low half sealed') {
        Write-Host 'FAIL: the low half was not sealed after secondaries started.'
        $result = 1
    }
    if ($text -notmatch 'Boot info: 0x[0-9a-f]+, version 2, 272 bytes') {
        Write-Host 'FAIL: BootInfo v2 was not validated.'
        $result = 1
    }
    if ($text -notmatch 'higher half: physmap 0xffffc00000000000-0x[0-9a-f]+ \(\d+ GiB\), boot root 0x[0-9a-f]+, kernel root 0x[0-9a-f]+') {
        Write-Host 'FAIL: higher-half geometry was not reported.'
        $result = 1
    }
    if ($text -notmatch 'running at 0x[0-9a-f]+, inside the higher-half image: yes') {
        Write-Host 'FAIL: the kernel is not executing from the higher-half image.'
        $result = 1
    }
    if ($text -notmatch 'PCIe: \d+ functions on \d+ buses across \d+ segments') {
        Write-Host 'FAIL: PCIe ECAM enumeration did not complete.'
        $result = 1
    }
    if ($text -notmatch 'ACPI XSDT|ACPI RSDT') {
        Write-Host 'FAIL: ACPI root table was not reported.'
        $result = 1
    }
    if ($text -notmatch 'RSDP .*checksum ok') {
        Write-Host 'FAIL: kernel did not accept the RSDP checksum.'
        $result = 1
    }
    if ($text -notmatch 'Running on ACPI CPU interface') {
        Write-Host 'FAIL: running CPU did not match a MADT entry.'
        $result = 1
    }
    if ($text -notmatch 'Exceptions: VBAR_EL\d 0x[0-9a-f]+, table aligned: yes') {
        Write-Host 'FAIL: exception vectors were not installed or are misaligned.'
        $result = 1
    }
    if ($Framebuffer -and $text -notmatch 'Framebuffer console: active') {
        Write-Host 'FAIL: framebuffer console did not activate.'
        $result = 1
    }
    if ($text -notmatch 'GICD: id \d+, base 0x[0-9a-f]+') {
        Write-Host 'FAIL: GIC distributor was not decoded.'
        $result = 1
    }
    if ($text -notmatch 'GIC: GICv3\+ distributor 0x[0-9a-f]+, redistributor 0x[0-9a-f]+, timer PPI \d+') {
        Write-Host 'FAIL: GIC was not initialized.'
        $result = 1
    }
    if ($text -notmatch 'Timer: CNTV one-shot, 1000000000 Hz counter, 1 ns resolution') {
        Write-Host 'FAIL: one-shot generic timer was not initialized.'
        $result = 1
    }
    if ($text -notmatch 'Physical self-test: PASS \([^)]*reservations\)') {
        Write-Host 'FAIL: physical allocator self-test did not include reservation checks.'
        $result = 1
    }
    if ($text -notmatch 'Heap self-test: PASS') {
        Write-Host 'FAIL: heap self-test did not pass.'
        $result = 1
    }
    if ($text -notmatch "SMP: $Cpus discovered, $Cpus expected online") {
        Write-Host 'FAIL: MADT discovery did not match the configured CPU count.'
        $result = 1
    }
    for ($core = 1; $core -lt $Cpus; $core++) {
        if ($text -notmatch "core ${core}: online, MPIDR 0x[0-9a-f]+, startup \d+ us") {
            Write-Host "FAIL: secondary core $core did not report online."
            $result = 1
        }
    }
    $expectedIpis = $Cpus - 1
    if ($text -notmatch "IPI: $expectedIpis secondaries answered a ping/pong round trip") {
        Write-Host 'FAIL: not every online secondary answered an IPI.'
        $result = 1
    }
    if ($text -notmatch 'Scheduler: EEVDF, 11 kernel workers, 16 KiB stacks with guard pages, 4000 us slice request, 1000-ms demo') {
        Write-Host 'FAIL: EEVDF scheduler was not configured as expected.'
        $result = 1
    }
    $expectedClusters = if ($TwoCluster) { 2 } else { 1 }
    if ($text -notmatch "Sched: $Cpus per-core run queues, one-shot CNTV events, $expectedClusters clusters?, cluster-aware stealing") {
        Write-Host "FAIL: $Cpus per-core run queues with $expectedClusters cluster(s) were not reported."
        $result = 1
    }
    for ($core = 0; $core -lt $Cpus; $core++) {
        if ($text -notmatch "Scheduler: core $core demo complete, [1-9][0-9]* ticks, [1-9][0-9]* context switches") {
            Write-Host "FAIL: scheduler demo did not complete on core $core."
            $result = 1
        }
    }
    $fpOk = ([regex]::Matches($text, 'fp ok')).Count
    if ($fpOk -ne 11) {
        Write-Host "FAIL: $fpOk of 11 kernel worker FP states matched the iteration count."
        $result = 1
    }
    if ($text -notmatch 'SMP: concurrency verified') {
        Write-Host 'FAIL: concurrent execution on the online cores was not verified.'
        $result = 1
    }
    if ($text -notmatch 'Sched: [1-9][0-9]* steals, [1-9][0-9]* migrations \(\d+ same-cluster, \d+ cross-cluster\), \d+ balance SGIs') {
        Write-Host 'FAIL: work stealing and migration counters were not reported.'
        $result = 1
    }
    if ($TwoCluster -and $text -notmatch '\(\d+ same-cluster, [1-9][0-9]* cross-cluster\)') {
        Write-Host 'FAIL: no cross-cluster migration occurred with two clusters.'
        $result = 1
    }
    if ($text -notmatch 'Sched: one-shot timer: [1-9][0-9]* events armed, [1-9][0-9]* idle disarms \(tickless\), [1-9][0-9]* IRQ masks') {
        Write-Host 'FAIL: one-shot timer events and tickless-idle disarms were not reported.'
        $result = 1
    }
    for ($core = 0; $core -lt $Cpus; $core++) {
        if ($text -notmatch "core $core`: work [1-9][0-9]*, ticks [1-9][0-9]*, switches [1-9][0-9]*") {
            Write-Host "FAIL: core $core did not report scheduler work, timer activity and switches."
            $result = 1
        }
    }
    if ($Debug) {
        if ($text -notmatch 'Sched: invariant checker: [1-9][0-9]* checks, 0 violations') {
            Write-Host 'FAIL: the debug invariant checker did not run cleanly.'
            $result = 1
        }
        if ($text -match 'INVARIANT VIOLATION') {
            Write-Host 'FAIL: the debug invariant checker reported a violation.'
            $result = 1
        }
    } elseif ($text -notmatch 'Sched: invariant checker: disabled in release build') {
        Write-Host 'FAIL: the release build did not state that the invariant checker is disabled.'
        $result = 1
    }
    if ($text -notmatch 'Scheduler: boot core handing off to the user phase') {
        Write-Host 'FAIL: the boot core did not hand off to the user phase.'
        $result = 1
    }
    if ($text -notmatch 'Userspace: 6 processes, 8 threads, \d+ pages, 16 KiB user stacks') {
        Write-Host 'FAIL: the 0.0.6 user processes and threads were not loaded.'
        $result = 1
    }
    foreach ($line in @(
        'init: hello from EL0',
        'init: syscall ABI 0x00010000',
        'init: exiting with status 0',
        'ipc: receiver waiting',
        'ipc: receiver got ping',
        'ipc: receiver sent pong',
        'ipc: sender sending ping',
        'ipc: sender got pong',
        'threads: A online',
        'threads: B online',
        'threads: A done',
        'threads: B done',
        'fault: reading an unmapped address',
        'latency: long-slice hog done',
        'latency: short-slice sleeper done'
    )) {
        if ($text -notmatch [regex]::Escape($line)) {
            Write-Host "FAIL: expected user output missing: $line"
            $result = 1
        }
    }
    if ($text -notmatch 'pid 6 tid 6: wake n [1-9][0-9]*, p50 <= \d+ ns, p99 <= \d+ ns, max \d+ ns; run delay n \d+, p50 <= \d+ ns, p99 <= \d+ ns') {
        Write-Host 'FAIL: the short-slice sleeper did not record wake-latency samples.'
        $result = 1
    }
    if ($text -notmatch 'pid 6 tid 7: wake n \d+, .*run delay n [1-9][0-9]*, p50 <= \d+ ns, p99 <= \d+ ns') {
        Write-Host 'FAIL: the long-slice hog did not record run-delay samples.'
        $result = 1
    }
    if ($text -notmatch 'Sched: latency figures are QEMU-TCG diagnostics, not performance claims') {
        Write-Host 'FAIL: the latency report was not labelled as QEMU-TCG diagnostics.'
        $result = 1
    }
    if ($text -notmatch 'Userspace: pid 4 fault \(ESR 0x[0-9a-f]+, FAR 0x0000000000001000\), terminating process') {
        Write-Host 'FAIL: the user fault process was not terminated with the expected data abort.'
        $result = 1
    }
    if ($text -notmatch 'Userspace: summary 6 processes, 0 threads') {
        Write-Host 'FAIL: the user phase did not reap every process and thread.'
        $result = 1
    }
    foreach ($processId in 1, 2, 3, 5, 6) {
        if ($text -notmatch "pid ${processId}: exited, status 0, 0 user pages") {
            Write-Host "FAIL: pid $processId did not exit cleanly."
            $result = 1
        }
    }
    if ($text -notmatch 'pid 4: terminated by fault, status 139, 0 user pages') {
        Write-Host 'FAIL: the faulting process was not recorded as terminated.'
        $result = 1
    }
    if ($text -notmatch 'PSCI: compliant yes') {
        Write-Host 'FAIL: FADT did not report PSCI compliance.'
        $result = 1
    }
    if ($text -notmatch 'QEMU: requesting PSCI SYSTEM_OFF') {
        Write-Host 'FAIL: kernel did not request PSCI SYSTEM_OFF.'
        $result = 1
    }
    if ($text -notmatch 'Entry exception level: EL[12], running at EL1') {
        Write-Host 'FAIL: kernel did not report an EL1 execution level.'
        $result = 1
    }
    if ($OccupyLegacy) {
        if ($text -notmatch 'Relocation test: occupied legacy kernel address 0x0000000040200000') {
            Write-Host 'FAIL: loader did not occupy the legacy kernel address.'
            $result = 1
        }
        if ($text -match 'Kernel entry: 0x0000000040200000') {
            Write-Host 'FAIL: kernel was still entered at the legacy address.'
            $result = 1
        }
    }
    if ($ExpectEl2 -and $text -notmatch 'Entry exception level: EL2, running at EL1') {
        Write-Host 'FAIL: kernel did not drop from EL2 to EL1.'
        $result = 1
    }
    if ($result -eq 0) {
        Write-Host ''
        Write-Host 'PASS: kernel reached the success marker and powered off.'
    }
}

if ($result -ne 0 -and (Test-Path $log) -and -not $KeepLog) {
    $failed = Join-Path $root 'build/serial.failed.log'
    Copy-Item -Force $log $failed
    Write-Host "Failure log kept at: $failed"
}

exit $result
