#!/usr/bin/env python3
"""Inspect raw ACPI table dumps captured from a machine.

This is a research/validation helper, not part of the boot path. It decodes
the ACPI tables SketchOS currently cares about so the kernel parsers can be
checked against real firmware data.
"""

from __future__ import annotations

import struct
import sys
from pathlib import Path

TYPE_NAMES = {
    0x00: "Local APIC",
    0x01: "IO APIC",
    0x02: "Interrupt Source Override",
    0x03: "NMI Source",
    0x04: "Local APIC NMI",
    0x05: "Local APIC Address Override",
    0x06: "IO SAPIC",
    0x07: "Local SAPIC",
    0x08: "Platform Interrupt Sources",
    0x09: "Local x2APIC",
    0x0A: "Local x2APIC NMI",
    0x0B: "GICC",
    0x0C: "GICD",
    0x0D: "GIC MSI Frame",
    0x0E: "GIC Redistributor",
    0x0F: "GIC ITS",
    0x10: "Multiprocessor Wakeup",
    0x11: "Core PIC",
    0x12: "Legacy IO PIC",
    0x13: "Hypervisor Vendor ID",
}


def u8(b: bytes, o: int) -> int:
    return b[o]


def u16(b: bytes, o: int) -> int:
    return struct.unpack_from("<H", b, o)[0]


def u32(b: bytes, o: int) -> int:
    return struct.unpack_from("<I", b, o)[0]


def u64(b: bytes, o: int) -> int:
    return struct.unpack_from("<Q", b, o)[0]


def header(table: bytes) -> None:
    length = u32(table, 4)
    print(
        f"    signature={table[:4].decode('ascii', 'replace')!r} "
        f"length={length} rev={u8(table, 8)} "
        f"checksum={u8(table, 9):#04x} "
        f"oem={table[10:16]!r} table={table[16:24]!r}"
    )
    print(
        f"    oem_rev={u32(table, 24):#x} creator={table[28:32]!r} "
        f"creator_rev={u32(table, 32):#x}"
    )


def parse_apic(table: bytes) -> None:
    header(table)
    print(f"    local_interrupt_controller={u32(table, 36):#x} flags={u32(table, 40):#x}")
    offset = 44
    index = 0
    while offset + 2 <= len(table):
        etype = u8(table, offset)
        elen = u8(table, offset + 1)
        if elen < 2 or offset + elen > len(table):
            print(f"    [{index}] truncated entry type={etype:#x} len={elen}")
            break
        name = TYPE_NAMES.get(etype, f"type {etype:#x}")
        if etype == 0x0B and elen >= 76:
            cpu_if = u32(table, offset + 4)
            uid = u32(table, offset + 8)
            flags = u32(table, offset + 12)
            mpidr = u64(table, offset + 68)
            gicc_base = u64(table, offset + 32)
            gicr_base = u64(table, offset + 60)
            efficiency = u8(table, offset + 76)
            print(
                f"    [{index}] GICC cpu_if={cpu_if} uid={uid} enabled={bool(flags & 1)} "
                f"gicc={gicc_base:#x} gicr={gicr_base:#x} mpidr={mpidr:#x} "
                f"eff_class={efficiency}"
            )
        elif etype == 0x0C and elen >= 24:
            print(
                f"    [{index}] GICD id={u32(table, offset + 4)} "
                f"base={u64(table, offset + 8):#x} version={u8(table, offset + 20)}"
            )
        elif etype == 0x0E and elen >= 16:
            print(
                f"    [{index}] GICR base={u64(table, offset + 4):#x} "
                f"length={u32(table, offset + 12):#x}"
            )
        elif etype == 0x0F and elen >= 20:
            print(
                f"    [{index}] GIC ITS id={u32(table, offset + 4)} "
                f"base={u64(table, offset + 8):#x}"
            )
        else:
            print(f"    [{index}] {name} len={elen}")
        offset += elen
        index += 1


def parse_facp(table: bytes) -> None:
    header(table)
    print(
        f"    firmware_ctrl={u32(table, 36):#x} dsdt={u32(table, 40):#x} "
        f"x_dsdt={u64(table, 148):#x}" if len(table) >= 156 else ""
    )
    print(f"    flags={u32(table, 112):#x}" if len(table) >= 116 else "")


def parse_mcfg(table: bytes) -> None:
    header(table)
    offset = 44
    index = 0
    while offset + 16 <= len(table):
        print(
            f"    [{index}] base={u64(table, offset):#x} "
            f"segment={u16(table, offset + 8):#x} "
            f"bus={u8(table, offset + 10):#x}-{u8(table, offset + 11):#x}"
        )
        offset += 16
        index += 1


def parse_gtdt(table: bytes) -> None:
    header(table)
    print(f"    cnt_control_base={u64(table, 36):#x} flags={u32(table, 52):#x}")
    print(
        f"    sec_el1_gsiv={u32(table, 48)} "
        f"ns_el1_gsiv={u32(table, 56)} "
        f"virtual_gsiv={u32(table, 64)} "
        f"ns_el2_gsiv={u32(table, 72)}"
    )


def parse_pptt(table: bytes) -> None:
    header(table)
    offset = 36
    index = 0
    while offset + 4 <= len(table):
        ptype = u8(table, offset)
        plen = u8(table, offset + 1)
        if plen < 4 or offset + plen > len(table):
            print(f"    [{index}] truncated type={ptype} len={plen}")
            break
        if ptype == 0:
            flags = u32(table, offset + 4)
            parent = u32(table, offset + 8)
            uid = u32(table, offset + 12)
            print(
                f"    [{index}] processor uid={uid} parent={parent} "
                f"physical_package={bool(flags & 1)} "
                f"acpi_processor={bool(flags & 2)} "
                f"thread={bool(flags & 8)} node={bool(flags & 0x10)}"
            )
        elif ptype == 1:
            size = u32(table, offset + 8)
            sets = u16(table, offset + 12)
            assoc = u8(table, offset + 14)
            attrs = u8(table, offset + 15)
            line = u16(table, offset + 16)
            cache_type = attrs & 0x1F
            print(
                f"    [{index}] cache type={cache_type} size={size} "
                f"sets={sets} assoc={assoc} line={line}"
            )
        else:
            print(f"    [{index}] type={ptype} len={plen}")
        offset += plen
        index += 1


def parse_dbg2(table: bytes) -> None:
    header(table)
    offset = 44
    index = 0
    while offset + 28 <= len(table):
        dtype = u8(table, offset)
        dlen = u16(table, offset + 1)
        if dlen < 28 or offset + dlen > len(table):
            print(f"    [{index}] truncated type={dtype} len={dlen}")
            break
        regs = u32(table, offset + 6)
        name_len = u16(table, offset + 10)
        name_off = u16(table, offset + 12)
        port_type = u16(table, offset + 18)
        port_subtype = u16(table, offset + 20)
        gas_off = u16(table, offset + 24)
        name = ""
        if name_len and name_off + name_len <= len(table):
            name = table[name_off : name_off + name_len].decode("ascii", "replace").rstrip("\0")
        base = None
        if regs and gas_off + 12 <= len(table):
            base = u64(table, offset + gas_off + 4)
        print(
            f"    [{index}] type={dtype} regs={regs} port_type={port_type:#x} "
            f"subtype={port_subtype:#x} name={name!r} base={base if base is None else hex(base)}"
        )
        offset += dlen
        index += 1


def parse_spcr(table: bytes) -> None:
    header(table)
    interface = u8(table, 36)
    base = u64(table, 44)
    print(
        f"    interface={interface:#x} base={base:#x} "
        f"interrupt_type={u8(table, 52):#x} irq={u8(table, 53)} "
        f"gsiv={u32(table, 54)} baud={u8(table, 58):#x} "
        f"parity={u8(table, 59)} stop={u8(table, 60)} flow={u8(table, 61)}"
    )


PARSERS = {
    "APIC": parse_apic,
    "FACP": parse_facp,
    "MCFG": parse_mcfg,
    "GTDT": parse_gtdt,
    "PPTT": parse_pptt,
    "DBG2": parse_dbg2,
    "SPCR": parse_spcr,
}


def main() -> int:
    try:
        sys.stdout.reconfigure(errors="replace")
    except AttributeError:
        pass
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[1] / "hardware"
    for path in sorted(root.rglob("*.dat")):
        sig = path.stem[:4].upper()
        print(f"=== {path.name} ===")
        try:
            data = path.read_bytes()
        except OSError as exc:
            print(f"    {exc}")
            continue
        if data[:4].decode("ascii", "replace") != sig:
            print(f"    unexpected signature {data[:4]!r}")
        parser = PARSERS.get(sig)
        if parser is None:
            print(f"    {len(data)} bytes, no detailed parser")
            header(data)
        else:
            try:
                parser(data)
            except (IndexError, struct.error) as exc:
                print(f"    parse error: {exc}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
