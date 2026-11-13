# SketchOS boot ABI, version 2

This document specifies the handoff between the SketchOS UEFI loader and the
SketchOS kernel. The authoritative definitions live in
[`boot/protocol/src/lib.rs`](../boot/protocol/src/lib.rs) and are shared by both
sides as the `sketchos-bootinfo` crate.

The ABI is a single, versioned, C-layout record passed in `x0`. It is `no_std`,
dependency-free, and contains only fixed-size values and physical addresses.

## Design constraints

- The loader is a UEFI application and runs with Boot Services available.
- The kernel is a separate freestanding ELF image and runs with Boot Services
  gone.
- Since 0.0.4 physical and virtual addresses are **different**. The loader
  builds and describes two translation regimes:
  - **TTBR0** — a bootstrap identity map of the kernel image and the boot
    record, used only by `boot.S` while it clears `.bss` and installs the real
    roots.
  - **TTBR1** — the higher-half kernel image (`KERNEL_VA_BASE + physical`) plus
    a linear physical map (`PHYSMAP_BASE + physical`). After the switch, every
    access to a physical byte, including MMIO and firmware tables, goes through
    the physical map.
- Every pointer in the record is a **physical** address. The kernel never
  dereferences a physical address directly; it adds `PHYSMAP_BASE` first.
- The record and the memory-map array are allocated as `EfiLoaderData` pages.
  They survive `ExitBootServices` and are described as loader data by the final
  memory map.
- The kernel must not call any UEFI Boot Service or Runtime Service. The
  handoff record is the only channel.

The constants that define the split are shared:

| Constant | Value | Meaning |
| --- | --- | --- |
| `KERNEL_VA_BASE` | `0xFFFF_8000_0000_0000` | `physical + KERNEL_VA_BASE` is the kernel image alias |
| `PHYSMAP_BASE` | `0xFFFF_C000_0000_0000` | `physical + PHYSMAP_BASE` is the linear map alias |
| `MAX_PHYS` | 512 GiB | Highest physical address the kernel tables describe |
| `PAGE_SIZE` | 4096 | Page granule for both regimes |

The 48-bit virtual address space is split evenly: `TCR_EL1.T0SZ = T1SZ = 16`.
A 4 KiB granule with four levels gives 48 bits per regime.

## Entry contract

When the loader branches to the kernel:

| Item | Value |
| --- | --- |
| Argument register | `x0` = physical address of `BootInfo` |
| Exception level | EL1 or EL2 |
| MMU | Enabled under the **firmware** identity map, caches enabled |
| Loader tables | Built and described, but not yet installed |
| Boot Services | Exited; no service may be called |
| Stack | Not provided by the loader; `boot.S` installs `__stack_top` |
| Return | The kernel must not return |

`arch/aarch64/boot.S` immediately masks all asynchronous exceptions with
`daifset #0xf`, reads the translation plan out of the record, clears `.bss`
(including the 64 KiB early stack), installs `TTBR0_EL1`, `TTBR1_EL1`,
`TCR_EL1` and `MAIR_EL1` from the record, sets `SCTLR_EL1.M/C/I` without
`WXN`, invalidates the firmware TLB, and then:

- **EL2 entry:** programs `HCR_EL2`, `CNTHCTL_EL2`, `CPTR_EL2` and
  `ICC_SRE_EL2`, then `eret`s to EL1h at the physical `el1_entry`.
- **EL1 entry:** installs the same state and branches to `el1_entry` directly.

`el1_entry` enables FP/SIMD (`CPACR_EL1.FPEN = 0b11`), moves the stack and the
continuation address into the higher half using the `kernel_va_start -
kernel_phys_start` bias, installs the kernel vector table, and calls
`kmain(boot_info_physical, entry_level)`. If `kmain` ever returned, the entry
code parks the core.

Other general-purpose registers, SIMD/FP state, and system-register state are
not part of the ABI beyond what is listed above. The kernel installs its own
vector table; the firmware's `VBAR_EL1` is not reused.

## Record layout

`BootInfo` is `#[repr(C)]`, little-endian, 8-byte aligned, and **272 bytes** in
version 2. Offsets are from the start of the record.

| Offset | Size | Field | Meaning |
| ---: | ---: | --- | --- |
| 0 | 8 | `magic` | `"SKBOOT01"` as a little-endian `u64` (`0x3130_544f_4f42_4b53`) |
| 8 | 4 | `version` | `2` |
| 12 | 4 | `size` | Size of this record in bytes (272 for version 2) |
| 16 | 8 | `flags` | `FLAG_*` bits |
| 24 | 24 | `memory_map` | `MemoryRegion` describing the descriptor array |
| 48 | 8 | `rsdp_address` | Physical address of the ACPI RSDP, or zero |
| 56 | 8 | `kernel_phys_start` | First physical byte of the loaded kernel image |
| 64 | 8 | `kernel_phys_end` | First byte past the kernel image |
| 72 | 8 | `loader_phys_start` | First byte of the loader image, or zero |
| 80 | 8 | `loader_phys_end` | First byte past the loader image, or zero |
| 88 | 8 | `uart_base` | Debug UART physical base, or zero |
| 96 | 4 | `uart_kind` | One of the `uart_kind` constants |
| 100 | 4 | `firmware_revision` | `EFI_SYSTEM_TABLE.Revision` |
| 104 | 32 | `framebuffer` | `FramebufferInfo` |
| 136 | 64 | `firmware_vendor` | ASCII, NUL padded, truncated to 63 characters |
| 200 | 6 | `acpi_oem_id` | Raw RSDP OEM ID bytes |
| 206 | 1 | `acpi_revision` | RSDP revision |
| 207 | 1 | `_reserved` | Zero |
| 208 | 8 | `ttbr0_boot_root` | Physical address of the bootstrap identity root |
| 216 | 8 | `ttbr1_root` | Physical address of the higher-half root |
| 224 | 8 | `tcr` | `TCR_EL1` value the loader's tables were built for |
| 232 | 8 | `mair` | `MAIR_EL1` value matching the attribute indices |
| 240 | 8 | `kernel_va_start` | Inclusive higher-half virtual start of the image |
| 248 | 8 | `kernel_va_end` | Exclusive higher-half virtual end of the image |
| 256 | 8 | `physmap_base` | Base of the linear physical map (`PHYSMAP_BASE`) |
| 264 | 8 | `physmap_size` | Bytes covered by the linear map, 1 GiB aligned |

The crate asserts `size_of::<BootInfo>() == 272` at compile time, so an
accidental layout change is a build error.

The translation fields are valid only when `FLAG_HIGHER_HALF` is set. A version
2 consumer must reject a record without it, because the kernel's entry code
cannot run in the higher half for a 0.0.3-style identity-only handoff.

### `MemoryRegion` (24 bytes)

| Offset | Size | Field | Meaning |
| ---: | ---: | --- | --- |
| 0 | 8 | `address` | Physical address of the first descriptor |
| 8 | 8 | `count` | Number of valid descriptors |
| 16 | 4 | `entry_size` | Size of one descriptor in bytes (`32` in version 2) |
| 20 | 4 | `_reserved` | Zero |

The loader allocates **capacity** for more descriptors than the final map needs,
then writes the authoritative `count` after `ExitBootServices`. Only the first
`count` entries are valid. The rest of the allocation is unused padding.

The array is mapped page-rounded in both the loader's root and the kernel's
final map: `round_up(count * 32, 4096)`. The array itself is only 8-byte
aligned, so a page-rounded span can extend past the last descriptor into the
rest of the loader allocation.

### `MemoryDescriptor` (32 bytes)

| Offset | Size | Field | Meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | `kind` | Normalized memory kind |
| 4 | 4 | `_reserved` | Zero |
| 8 | 8 | `physical_start` | First physical byte (4 KiB aligned) |
| 16 | 8 | `page_count` | Length in 4 KiB pages |
| 24 | 8 | `attributes` | Raw UEFI capability attributes |

`size_bytes()` is `page_count * 4096`; `end()` is checked-add and returns
`None` on overflow. The kernel must never assume ranges are sorted, contiguous,
or non-overlapping.

### `FramebufferInfo` (32 bytes)

| Offset | Size | Field | Meaning |
| ---: | ---: | --- | --- |
| 0 | 8 | `address` | Physical base of the linear framebuffer |
| 8 | 4 | `width` | Visible width in pixels |
| 12 | 4 | `height` | Visible height in pixels |
| 16 | 4 | `stride` | Pixels per scan line (may exceed `width`) |
| 20 | 4 | `pixel_format` | `pixel_format` constant |
| 24 | 8 | `size` | Mapped framebuffer size in bytes |

When `FLAG_FRAMEBUFFER` is set, the kernel validates that:

- `address`, `width`, and `height` are non-zero,
- `stride >= width`,
- `size >= stride * height * 4`,
- `address + size` is inside the physical map.

If any check fails, the kernel clears `FLAG_FRAMEBUFFER` and continues booting.
It does not refuse the handoff for an unusable optional description.

## Flags

| Bit | Constant | Meaning |
| ---: | --- | --- |
| 0 | `FLAG_MEMORY_MAP` | `memory_map` points at a valid descriptor array. Required. |
| 1 | `FLAG_RSDP_VALID` | `rsdp_address` is non-zero and the loader validated its checksums. |
| 2 | `FLAG_FRAMEBUFFER` | `framebuffer` contains a usable GOP description. |
| 3 | `FLAG_UART` | `uart_base` / `uart_kind` describe a debug UART. |
| 4 | `FLAG_IDENTITY_MAP` | Firmware handed off under its own identity map with caches enabled. |
| 5 | `FLAG_QEMU_VIRT` | The platform was identified as QEMU `virt` from the ACPI OEM ID. |
| 6 | `FLAG_HIGHER_HALF` | The trailing translation fields are valid and the loader described a higher-half map. Required. |

Unknown flag bits are ignored by version-2 consumers. A consumer must not assume
that a flag is present just because the corresponding field is non-zero; the
flag is the authority.

`FLAG_IDENTITY_MAP` describes the map that is active at the entry point, not the
map the kernel runs on afterwards. It remains set because firmware always enters
the kernel with its own identity map.

`FLAG_QEMU_VIRT` is a platform-identification hint, not a capability. It is set
when the RSDP OEM ID is exactly `BOCHS `, which is how QEMU's ACPI tables
identify themselves. The kernel uses it only to permit the QEMU-only PSCI
power-off path.

### Memory kinds

The standard values mirror UEFI `EFI_MEMORY_TYPE` so the loader conversion is
mechanical.

| Value | Name | Kernel use (0.0.5 and 0.0.6) |
| ---: | --- | --- |
| 0 | `RESERVED` | No |
| 1 | `LOADER_CODE` | Reported; reserved |
| 2 | `LOADER_DATA` | Reported and reserved (includes the kernel image and handoff pages) |
| 3 | `BOOT_SERVICES_CODE` | Reported; reserved |
| 4 | `BOOT_SERVICES_DATA` | Reported; reserved |
| 5 | `RUNTIME_SERVICES_CODE` | No |
| 6 | `RUNTIME_SERVICES_DATA` | No |
| 7 | `CONVENTIONAL` | Managed by the physical allocator; allocatable |
| 8 | `UNUSABLE` | No |
| 9 | `ACPI_RECLAIM` | Reported; reserved |
| 10 | `ACPI_NVS` | No |
| 11 | `MMIO` | No |
| 12 | `MMIO_PORT_SPACE` | No |
| 13 | `PAL_CODE` | No |
| 14 | `PERSISTENT` | No |
| 255 | `UNKNOWN` | No |

"Reported" means the boot inventory counts the range. In 0.0.4 through 0.0.6 only
`CONVENTIONAL` descriptors are handed to the physical allocator. Every other
kind is either never managed or explicitly reserved, so it cannot be returned by
an allocation. All RAM-like kinds are promoted to Normal write-back memory in
the physical map; everything else starts as Device-nGnRnE. See
[memory-management.md](memory-management.md).

### UART kinds

| Value | Name | Kernel behavior (0.0.5 and 0.0.6) |
| ---: | --- | --- |
| 0 | `NONE` | No console |
| 1 | `PL011` | Driven at 115200 8N1, polling only |
| 2 | `UART16550` | Recorded, not driven |
| 3 | `SBSA` | Recorded, not driven |
| 4 | `OTHER` | Recorded, not driven |

The kernel initializes a console only when `FLAG_UART` is set and
`uart_kind == PL011`. `uart_base` must be non-zero and 4-byte aligned, otherwise
the kernel clears `FLAG_UART`. Driving an unknown register layout is considered
worse than staying silent. The UART page is mapped Device at
`PHYSMAP_BASE + uart_base`.

### Pixel formats

| Value | Name |
| ---: | --- |
| 0 | `UNKNOWN` |
| 1 | `RGB` |
| 2 | `BGR` |
| 3 | `BITMASK` |

## Loader sequence

The loader performs these steps in order. The ordering is part of the ABI's
safety argument.

1. Initialize UEFI helpers and disable the firmware watchdog.
2. Read the ACPI RSDP **only** from the UEFI configuration table, preferring
   the ACPI 2.0+ entry and falling back to ACPI 1.0.
3. Validate the RSDP's 20-byte checksum and, for revision 2+, the extended
   checksum and length. Reject a missing root table.
4. Walk the XSDT (revision 2+) or RSDT, verify the root table checksum and
   length, and collect the table count. Locate `SPCR` and record its interface,
   base address, and normalized UART kind.
5. Query the GOP non-exclusively. If the mode has a linear framebuffer with a
   plausible stride and size, record it. `BltOnly` and unusable modes are
   reported as unavailable.
6. Read `\EFI\SketchOS\kernel.elf` into memory, validate the ELF header and
   program headers, and require:
   - ELF64, little-endian, AArch64,
   - `ET_DYN` (relocatable) or `ET_EXEC` (legacy fixed image),
   - at least one `PT_LOAD`,
   - no segment that is both writable and executable,
   - every segment page-aligned with `p_filesz <= p_memsz`,
   - a page-aligned load bias,
   - entry inside the loaded range,
   - a bounded total span (128 MiB),
   - for `ET_DYN`, a `PT_DYNAMIC` segment whose relocations are all
     `R_AARCH64_RELATIVE` or `R_AARCH64_NONE`.
7. `ET_DYN`: allocate a free physical range below 4 GiB with
   `AllocateType::MaxAddress`, using the page-rounded span. `ET_EXEC`: allocate
   the exact linked physical range. Zero the whole allocation, copy each
   segment's file bytes, and zero each `.bss` tail.
8. Apply relocations. The **target** is the physical destination
   (`phys_bias + r_offset`); the **value** is the higher-half address
   (`virt_bias + addend`). Addends are bound to the page-rounded image span so
   the linker's one-past-the-end section symbols remain valid.
9. Clean the data cache over the image and invalidate the instruction cache.
   This is required on real hardware after writing executable code with caches
   enabled.
10. Drop the filesystem, file buffer, and every protocol handle.
11. Allocate the `BootInfo` page, the memory-map descriptor array, and an 8 MiB
    page-table pool as `EfiLoaderData` with `AllocatePages`. Never free them.
12. Write the preliminary record, then call `ExitBootServices`.
13. Copy the final UEFI memory map into the preallocated array, translating
    unknown memory types to `UNKNOWN`.
14. Patch `memory_map.count` and `FLAG_MEMORY_MAP`. Compute `physmap_size` as
    the highest mapped physical address (kernel, loader pages, map, RSDP, UART,
    framebuffer, firmware descriptors) rounded up to 1 GiB and bounded by
    `MAX_PHYS`.
15. Build the bootstrap TTBR0 root: identity-map the kernel image and the boot
    record with `BOOT_RWX`. Build the TTBR1 root: map the whole physical range
    as Device, promote every RAM-like descriptor to Normal write-back, map the
    boot record and the map array Normal writable, map the kernel image
    PT_LOAD-by-PT_LOAD with `RX`/`RW`/`RO`, and map the UART Device and the
    framebuffer Normal non-cacheable.
16. Read `ID_AA64MMFR0_EL1` to derive the physical-address size and ASID width,
    compute `TCR_EL1` and `MAIR_EL1`, and write the translation fields.
17. Set `FLAG_HIGHER_HALF`, clean the page-table pool, boot record and map
    array to the point of coherency, issue `dsb sy` / `isb`, and branch to the
    kernel's **physical** entry with `x0` pointing at the record.

No allocation, protocol access, printing, or UEFI call occurs after step 12.
The loader does not call Runtime Services either.

### Translation state written by the loader

| Register | Value |
| --- | --- |
| `TCR_EL1` | `T0SZ = T1SZ = 16`, `TG0 = 4K`, `TG1 = 4K`, `IRGN/ORGN = WBWA`, `SH = inner shareable`, `EPD0 = EPD1 = 0`, `A1 = 0`, `IPS` from `ID_AA64MMFR0_EL1`, `AS` per ASID width |
| `MAIR_EL1` | `0x0044_00ff`: Attr0 Normal WB, Attr1 Device-nGnRnE, Attr2 Normal non-cacheable |
| `TTBR0_EL1` | Bootstrap identity root, ASID 0 |
| `TTBR1_EL1` | Higher-half root, ASID 0 |

`MAIR_VALUE` is asserted as `0x0044_00ff` in `sketchos-mm`. The observed
`TCR_EL1` on QEMU is `0x0000_0005_b510_3510`.

## Kernel validation

The kernel treats the loader as trusted but still validates every structural
field before dereferencing it. A bad record halts immediately; there is no
console yet at that point because the console address itself comes from the
record. All record and array accesses go through `phys_to_virt`.

Required checks:

- `BootInfo` pointer non-null and aligned to `align_of::<BootInfo>()`.
- `magic == BOOT_INFO_MAGIC`.
- `version == 2`. The kernel does **not** accept a different version merely
  because `size` looks compatible.
- `size == 272` (or a larger accepted version-2 record).
- `FLAG_MEMORY_MAP` and `FLAG_HIGHER_HALF` present.
- `ttbr1_root` and `ttbr0_boot_root` non-zero and page-aligned.
- `physmap_base == PHYSMAP_BASE`.
- `physmap_size` non-zero, page-aligned, at most `MAX_PHYS`, and not
  overflowing when added to the base.
- `kernel_phys_start != 0`, `kernel_phys_start < kernel_phys_end`, and
  `kernel_phys_end` inside the physical map.
- `kernel_va_start == KERNEL_VA_BASE + kernel_phys_start` and the same for
  `kernel_va_end`.
- `TCR_EL1` keeps both walks enabled (`EPD0 = EPD1 = 0`) and uses the 4 KiB
  granule for both regimes.
- `MAIR_EL1` is non-zero.
- Map address non-null and 8-byte aligned.
- `entry_size == 32`.
- `1 <= count <= 1_048_576`.
- `count * entry_size` and `address + map_bytes` do not overflow and are inside
  the physical map.

Optional descriptions are downgraded rather than fatal:

- `FLAG_UART` is cleared if the base or kind is unusable, or if a PL011 base is
  not 4-byte aligned.
- `FLAG_FRAMEBUFFER` is cleared if the geometry is inconsistent or outside the
  physical map.
- `FLAG_RSDP_VALID` is cleared if `rsdp_address` is zero.

The RSDP is **not** required to be 16-byte aligned. ACPI recommends 16-byte
alignment, but firmware is not required to honour it. QEMU/EDK2 places the RSDP
at `0xbcb43018` (8-byte aligned), and all reader accesses are unaligned-safe.
An earlier alignment check incorrectly rejected that RSDP; it was removed.

## Versioning rules

1. A layout change bumps `BOOT_INFO_VERSION`. Consumers reject versions they do
   not know.
2. Adding a flag bit is backward-compatible; old consumers ignore it.
3. Adding a memory kind value is backward-compatible; old consumers treat
   unrecognized values as `UNKNOWN`.
4. New trailing fields require a version bump and a size increase. Version 2
   consumers require the exact version and the full 272-byte layout; they do
   not parse a shorter prefix.
5. `entry_size` exists so a future version can grow `MemoryDescriptor` without
   moving the array. Version 2 requires exactly 32 bytes.
6. Pointer fields are physical addresses. A version-2 consumer reaches them
   only through the physical map.
7. The record and descriptor array are loader-owned and are never freed by the
   kernel.
8. Version 1 records are rejected. They describe an identity map and a 208-byte
   layout that a higher-half kernel cannot enter safely.

## QEMU example

From a successful 0.0.5 normal QEMU boot:

```text
Boot info: 0x00000000bcb40000, version 2, 272 bytes, flags 0x0000007b
Firmware: "EDK II", revision 0x00010000
CPU: EL1, MPIDR 0x0000000080000000, counter 1000000000 Hz, QEMU virt
Entry exception level: EL1, running at EL1
Kernel: physical 0x00000000bba29000-0x00000000bc52f000 (11288 KiB),
        virtual 0xffff8000bba29000-0xffff8000bc52f000
  running at 0xffff8000bba38288, inside the higher-half image: yes
  higher half: physmap 0xffffc00000000000-0xffffc000c0000000 (3 GiB),
                boot root 0x00000000bb229000, kernel root 0x00000000bb22f000
  TCR 0x00000005b5103510, MAIR 0x00000000004400ff
```

With `-Virtualization -ExpectEl2`, the same record is handled after entering at
EL2 and dropping to EL1.

`flags = 0x7b` sets bits 0, 1, 3, 4, 5 and 6: memory map, RSDP, UART,
firmware identity map, QEMU, and higher half. Bit 2 (framebuffer) is clear
because the smoke test runs QEMU with `-display none`.

The loader also reports the chosen image base and relocation count:

```text
Kernel allocation: 2822 pages at 0x00000000bba29000 (relocatable,
                   phys bias 0x00000000bba29000, virt bias 0xffff8000bba29000)
Kernel: 1494080 bytes, 5 segments, 11288 KiB physical at ...
Kernel image: ET_DYN, 60 relocations
Kernel entry: physical 0x00000000bba29000, virtual 0xffff8000bba29000
Kernel virtual range: 0xffff8000bba29000..0xffff8000bc52f000
```

Addresses, counts, startup times and relocation counts are firmware-dependent
and will differ on another machine or QEMU version. They are evidence, not
constants.
