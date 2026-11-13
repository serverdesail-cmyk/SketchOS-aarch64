#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

QEMU="${QEMU_AARCH64:-qemu-system-aarch64}"
if ! command -v "$QEMU" >/dev/null 2>&1; then
  echo "ERROR: qemu-system-aarch64 is not installed" >&2
  exit 2
fi

find_first() {
  local pattern="$1"
  find /usr/share \( -type f -o -type l \) -iname "$pattern" -print -quit 2>/dev/null || true
}

CODE="${AAVMF_CODE:-$(find_first AAVMF_CODE.fd)}"
VARS_TEMPLATE="${AAVMF_VARS:-$(find_first AAVMF_VARS.fd)}"
if [[ -z "$CODE" || -z "$VARS_TEMPLATE" ]]; then
  echo "ERROR: AArch64 UEFI firmware (AAVMF_CODE.fd/AAVMF_VARS.fd) was not found" >&2
  exit 2
fi
if [[ ! -s images/SketchOS-AArch64.iso ]]; then
  echo "ERROR: ISO is missing; run tools/build-iso.sh first" >&2
  exit 2
fi

mkdir -p build
VARS="$ROOT/build/aavmf-vars-smoke.fd"
LOG="$ROOT/build/qemu-smoke-serial.log"
cp "$VARS_TEMPLATE" "$VARS"
rm -f "$LOG"

set +e
timeout --signal=TERM --kill-after=10s 180s "$QEMU" \
  -machine virt,gic-version=3 \
  -boot order=d \
  -cpu max -smp 8 -m 2048 \
  -drive if=pflash,format=raw,readonly=on,file="$CODE" \
  -drive if=pflash,format=raw,file="$VARS" \
  -drive if=none,id=sketchos-cd,media=cdrom,format=raw,file="$ROOT/images/SketchOS-AArch64.iso" \
  -device virtio-scsi-pci,id=scsi \
  -device scsi-cd,drive=sketchos-cd \
  -display none -monitor none -serial "file:$LOG" -no-reboot
QEMU_STATUS=$?
set -e

if [[ $QEMU_STATUS -eq 124 || $QEMU_STATUS -eq 137 ]]; then
  echo "FAIL: QEMU timed out; the kernel did not power off" >&2
  cat "$LOG" 2>/dev/null || true
  exit 1
fi
if [[ ! -s "$LOG" ]]; then
  echo "FAIL: QEMU produced no serial output (status $QEMU_STATUS)" >&2
  exit 1
fi

require() {
  local pattern="$1"
  local description="$2"
  if ! grep -Eq "$pattern" "$LOG"; then
    echo "FAIL: $description" >&2
    cat "$LOG" >&2
    exit 1
  fi
}

require 'SketchOS UEFI loader 0\\.0\\.6 \(AArch64\)' 'UEFI loader banner missing'
require 'HANDOFF: ENTERING KERNEL' 'loader did not reach the kernel handoff'
require 'PCI DMA:' 'PCI DMA quiescence path did not run'
require 'SketchOS kernel 0\\.0\\.6 \(AArch64\)' 'kernel banner missing'
require 'MMU: higher half, TTBR1 0x[0-9a-f]+, TTBR0 boot 0x[0-9a-f]+' 'kernel did not install its own higher-half map'
require 'MMU: W\\^X audit clean' 'kernel W^X audit failed or did not run'
require 'MMU: address-space self-test passed' 'address-space self-test failed'
require 'MMU: fault self-test passed' 'recoverable-fault self-test failed'
require 'MMU: TTBR0 walks disabled; low half sealed' 'low-half sealing did not complete'
require 'Userspace: summary 6 processes, 0 threads' 'userspace process lifecycle test did not complete'
require 'QEMU: requesting PSCI SYSTEM_OFF' 'kernel did not request the QEMU shutdown path'

echo "PASS: ISO booted under AArch64 QEMU and completed the kernel smoke checks."
echo "Serial log: $LOG"
