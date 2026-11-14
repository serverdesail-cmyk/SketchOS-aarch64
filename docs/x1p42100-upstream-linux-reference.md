# X1P42100 HP OmniBook X14 — upstream Linux hardware reference

This document cross-checks SketchOS against the upstream Linux board description for the exact HP OmniBook X14 / Snapdragon X Plus family. It is a reference for understanding the platform, not permission to blindly hardcode addresses. The live ACPI DSDT/SSDT resources and power methods remain authoritative for this UEFI boot path.

## Primary upstream sources

- [Board DTS: x1p42100-hp-omnibook-x14.dts](https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/qcom/x1p42100-hp-omnibook-x14.dts)
- [Shared HP OmniBook X14 board description](https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/qcom/x1-hp-omnibook-x14.dtsi)
- [Purwa / X1P SoC description](https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/qcom/purwa.dtsi)
- [Hamoa common SoC description](https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/qcom/hamoa.dtsi)
- [Qualcomm GENI I2C driver](https://github.com/torvalds/linux/blob/master/drivers/i2c/busses/i2c-qcom-geni.c)
- [Qualcomm GENI serial driver](https://github.com/torvalds/linux/blob/master/drivers/tty/serial/qcom_geni_serial.c)
- [GENI Serial Engine common layer](https://github.com/torvalds/linux/blob/master/drivers/soc/qcom/qcom-geni-se.c)
- [Qualcomm SMMU support](https://github.com/torvalds/linux/tree/master/drivers/iommu/arm/arm-smmu)
- [Adreno DRM driver](https://github.com/torvalds/linux/tree/master/drivers/gpu/drm/msm)

## Board-level cross-checks

The upstream board description identifies the target as the X1P42100 HP OmniBook X14 and selects the Purwa SoC description.

| Function | Upstream board reference | SketchOS implication |
|---|---|---|
| Early serial | GENI UART21 at physical 0x00894000; board serial0 alias | Matches the SPCR/DBG2 base already recorded by SketchOS. The current polling-only GENI path is the right early-console approach. |
| UART interrupt | DTS describes UART21 with GIC_SPI 813 | The current ACPI inventory reports SPCR GSIV 386. These are not reconciled. Do not enable a UART interrupt until the raw SPCR/DBG2/DSDT resources are checked. |
| QUPv3 wrapper 0 | 0x00bc0000; M_AHB/S_AHB clocks; IOMMU stream 0x443 | Parent clock/reset/power dependencies must be initialized before I2C0. |
| QUPv3 wrapper 1 | 0x00ac0000; M_AHB/S_AHB clocks; IOMMU stream 0x123 | Parent clock/reset/power dependencies must be initialized before I2C8. |
| QUPv3 wrapper 2 | 0x008c0000; M_AHB/S_AHB clocks; IOMMU stream 0x456 | Contains the UART21 serial engine. Do not reprogram it during early console output. |
| I2C0 | 0x00b80000; DTS GIC_SPI 373 | Board connects the internal keyboard and touchpad here. |
| I2C8 | 0x00a80000; DTS GIC_SPI 800 | Board connects the touchscreen here. |
| Keyboard | HID-over-I2C at address 0x3a; GPIO 67 interrupt | Requires I2C0 PIO, TLMM GPIO/input IRQ, HID-over-I2C and the described supply/pinctrl sequencing. |
| Touchpad | HID-over-I2C at address 0x15; GPIO 3 interrupt | Requires the same I2C0/TLMM/power foundations. |
| Touchscreen | HID-over-I2C at address 0x10 on I2C8; GPIO 51 interrupt | Requires I2C8/TLMM/power foundations. |
| GPU | Adreno compatible string qcom,adreno-43030c00 and GMU x145.0 | This is not a generic Adreno framebuffer. It requires GMU, clocks, power domains, firmware and SMMU. |
| Wi-Fi/Bluetooth | WCN6855 family in the board description | Requires the correct PCIe/firmware/power path; no SketchOS network driver exists yet. |

The DTS describes the keyboard/touchpad/touchscreen interrupt pins as active-low. The ACPI GPIO resources and polarity must be confirmed from the laptop's actual AML before programming TLMM.

## Implementation consequences

1. Keep the current GENI UART path polling-only. It verifies the firmware-configured UART protocol and DMA mode before writing the FIFO; do not enable interrupts or reprogram clocks until the full QUP wrapper state is understood.
2. Implement the GENI I2C controller in FIFO/PIO mode first. The QUP wrappers have IOMMU stream IDs, so DMA mode must wait until the SMMU and DMA API are operational.
3. Build TLMM GPIO input/interrupt support before HID-over-I2C. The GPIO pin numbers above are cross-checks; ACPI resources must confirm them on the actual firmware path.
4. Implement HID-over-I2C only after the controller, clock, reset, power-resource and GPIO dependencies are represented in the device model.
5. Treat NVMe, USB, Wi-Fi and Adreno as later milestones requiring PCIe/xHCI, SMMU, clocks, power domains and firmware.

## Known discrepancy requiring raw ACPI

The board DTS describes UART21 at 0x00894000 with GIC_SPI 813, while the current ACPI summary in the SketchOS hardware inventory reports the SPCR UART base 0x00894000 with GSIV 386. The polling console does not depend on that interrupt, but an interrupt-driven UART driver must not choose either number by guesswork. Preserve the raw SPCR, DBG2 and DSDT/SSDT tables and resolve the discrepancy first.

## Source hierarchy

For actual hardware initialization use this order:
1. ACPI tables from the running laptop, including DSDT and every SSDT, for assigned resources, GPIOs, interrupts, power methods and device presence.
2. Official Qualcomm/Arm architectural documentation for register semantics and ordering.
3. Upstream Linux DTS and drivers for board topology, known working register sequences, clocks, resets and quirks.
4. Never use a register address, interrupt, clock ID, GPIO or stream ID solely because it appears in a different Snapdragon generation's source.
