# Raw ACPI tables for hardware bring-up

Add the original binary ACPI table blobs from the laptop here. These are required for static AML review and a correct uACPI integration.

Required:
- `DSDT.dat`
- every `SSDT*.dat` (there may be multiple tables with the same signature)
- `FACP.dat` / FADT
- `GTDT.dat`
- `SPCR.dat`
- `DBG2.dat`
- `IORT.dat`
- `MCFG.dat`
- `APIC.dat`
- `table-list.csv`

Preserve the binary tables exactly as exported. Do not upload screenshots in place of the blobs. Disassembled ASL files may be added alongside the binaries, but are not a replacement for them.

The previous export workspace was `C:\SketchOS\hardware\ACPI`. The raw tables can be copied from there into this directory and committed to the repository, avoiding the chat attachment-size limit. Include every SSDT, not just one.
