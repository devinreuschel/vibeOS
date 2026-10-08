QEMU `virt` device trees for the §11.5 host tests (DESIGN §1.5: dumpdtb output is data).

Each `.dtb` was written by `qemu-system-aarch64 -machine …,dumpdtb=` and compacted with `dtc` so the blob is the structure QEMU emitted, not the 1 MiB dump buffer.

| File | Command |
|---|---|
| `virt-8.2.dtb` | QEMU 8.2.2 `-machine virt,acpi=off,gic-version=3` |
| `virt-8.2-secure.dtb` | QEMU 8.2.2 `-machine virt,acpi=off,gic-version=3,secure=on` (disabled secure UART first) |
| `virt-current.dtb` | QEMU 9.2.1 `-machine virt,acpi=off,gic-version=3` (`/aliases/serial0`) |
| `reserved-both.dtb` | `reserved-both.dts`: FDT memreserve plus `/reserved-memory` |
| `reserved-two-reg.dtb` | two `reg` pairs on one `no-map` node, plus a memreserve |
| `reserved-32.dtb` | 32 `/reserved-memory` children, the table's cap |
| `reserved-33.dtb` | 33 `/reserved-memory` children, one past the cap |
| `reserved-overflow.dtb` | one `reg` whose start + size overflows |
| `reserved-deep.dtb` | path past 128 bytes and a nest past the depth cap, then `/reserved-memory` |
| `timer-5irq.dtb` | `timer-5irq.dts`: five-entry `arm,armv8-timer` (hyp-virt) |
| `two-uarts.dtb` | disabled PL011 first, no `/aliases` |
| `two-uarts-alias.dtb` | okay PL011 first, `/aliases/serial0` names `0x9000000` |
| `ecam-bus-range.dtb` | `ecam-bus-range.dts`: `pci-host-ecam-generic` with `bus-range = <0x10 0x1f>` |
