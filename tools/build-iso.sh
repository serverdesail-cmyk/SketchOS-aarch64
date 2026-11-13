#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET_KERNEL=aarch64-unknown-none
TARGET_UEFI=aarch64-unknown-uefi
OUT_KERNEL="target/$TARGET_KERNEL/release"
OUT_UEFI="target/$TARGET_UEFI/release"
ESP="$ROOT/build/esp"
ESP_IMG="$ROOT/build/sketchos-esp-fat.img"
STAGE="$ROOT/build/iso-root"

mkdir -p userspace/build
programs=(
  "init:userspace/init.rs:init.elf"
  "ipc_recv:userspace/ipc_recv.rs:ipc-recv.elf"
  "ipc_send:userspace/ipc_send.rs:ipc-send.elf"
  "faulter:userspace/faulter.rs:faulter.elf"
  "threads:userspace/threads.rs:threads.elf"
  "latency:userspace/latency.rs:latency.elf"
)
for item in "${programs[@]}"; do
  IFS=: read -r name source output <<< "$item"
  echo "==> Building userspace: $name"
  rustc --edition 2021 --crate-name "$name" --crate-type bin \
    --target "$TARGET_KERNEL" -O -C panic=abort -C debuginfo=0 \
    -C relocation-model=static -C link-arg=-Tuserspace/user.ld \
    -C link-arg=-z -C link-arg=max-page-size=4096 \
    -C link-arg=--build-id=none "$source" -o "userspace/build/$output"
done

echo "==> Building kernel"
cargo build --locked --release --target "$TARGET_KERNEL" -p sketchos-kernel
echo "==> Building UEFI loader"
cargo build --locked --release --target "$TARGET_UEFI" -p sketchos-uefi

test -f "$OUT_KERNEL/sketchos-kernel"
test -f "$OUT_UEFI/sketchos-uefi.efi"

rm -rf "$ESP" "$STAGE"
mkdir -p "$ESP/EFI/BOOT" "$ESP/EFI/SketchOS" "$STAGE"
cp "$OUT_UEFI/sketchos-uefi.efi" "$ESP/EFI/BOOT/BOOTAA64.EFI"
cp "$OUT_KERNEL/sketchos-kernel" "$ESP/EFI/SketchOS/kernel.elf"
cp -a "$ESP/." "$STAGE/"

# Create a plain FAT32 El Torito EFI System Partition image. This avoids
# embedding the project's MBR-partitioned USB image as an El Torito image.
truncate -s 64M "$ESP_IMG"
mkfs.vfat -F 32 -n SKETCHOS "$ESP_IMG"
mmd -i "$ESP_IMG" ::/EFI
mmd -i "$ESP_IMG" ::/EFI/BOOT
mmd -i "$ESP_IMG" ::/EFI/SketchOS
mcopy -i "$ESP_IMG" "$ESP/EFI/BOOT/BOOTAA64.EFI" ::/EFI/BOOT/BOOTAA64.EFI
mcopy -i "$ESP_IMG" "$ESP/EFI/SketchOS/kernel.elf" ::/EFI/SketchOS/kernel.elf
cp "$ESP_IMG" "$STAGE/esp.img"

mkdir -p images
xorriso -as mkisofs -R -J -V SKETCHOS_AARCH64 \
  -o images/SketchOS-AArch64.iso \
  -eltorito-alt-boot -e esp.img -no-emul-boot "$STAGE"

sha256sum images/SketchOS-AArch64.iso > images/SketchOS-AArch64.iso.sha256
printf 'ISO: %s\n' "$ROOT/images/SketchOS-AArch64.iso"
sha256sum "$ROOT/images/SketchOS-AArch64.iso"
