# Native Rust backend

The native edition starts at the root `Cargo.toml`. The older Tauri code is only in Git history; the historical audit documents describe that implementation. This application uses the installed Windows and NVIDIA drivers directly and does not use `NUCToolV2.dll` or WCF.

| Module | Responsibility |
| --- | --- |
| `src/main.rs` | GUI entry point and explicit read-only `--diagnose` mode. |
| `src/ui.rs` | Native Windows controls, keyboard navigation, DPI layout/scrolling, and message dispatch. |
| `src/fan_graph.rs` | Native GDI fan graph, mouse dragging, keyboard point editing, and neighbor/thermal validation. |
| `src/backend.rs` | One hardware worker, requests, inventory/sampling, power presets, fan and battery actions, restoration. |
| `src/machine.rs` | Rust COM/WMI inventory and exact platform gating. |
| `src/ec.rs` | The installed Uniwill ACPI driver protocol; no general-purpose register editor is exposed. |
| `src/gpu.rs` | NVIDIA's documented NVML functions loaded from the Windows system directory; queries only. |
| `src/model.rs` | Curve units, ranges, ordering, thermal floor, and power-input validation. |
| `src/journal.rs` | Recovery records, register allowlists, write verification, and rollback. |

## Driver communication

EC reads/writes use `\\.\ACPIDriver`, with IOCTLs `0x9C40A488` and `0x9C40A48C`. The input is a 32-bit little-endian address for a read, or address plus a 32-bit value for a write. Only one-byte register values are exposed internally. A successful write waits 6 ms. A read must return the expected four-byte output; failures propagate instead of becoming zero.

The only writable groups are CPU (`0x0783/84`), battery (`0x07B9`), and fan flags/table regions (`0x0751`, `0x0741`, `0x07C5/6`, and the six 16-byte regions beginning `0x0F00..0x0F50`). Recovery files are validated against the same allowlist, machine UUID, and BIOS version before use. EC access is enabled only for LAPKC71E with KCTGL357 BIOS and i7-11800H identity. This gate is a compatibility boundary, not a claim that physical write testing is complete.

CPU performance preset values come from `0x07A7/A8`, balanced values from `0x0730/31`, and power-saver values from `0x0734/35`, following the installed service's implementation. Zero/inconsistent preset data disables CPU writes. Manual limits must be positive, PL1 <= PL2, and within the performance-preset values. Those preset values are not described as absolute silicon limits.

The battery setter preserves bit 7 and writes a charge limit of 50–100 to the lower bits at `0x07B9`. The native application does not reproduce the vendor's separate battery-policy state machine; service reapplication can replace the override.

## Fan tables

CPU up/down/duty arrays begin at `0x0F00/10/20`; GPU arrays begin at `0x0F30/40/50`. Six arrays of 16 bytes are read from hardware. Native duty values 0–200 are presented as 0–100% in half-percent steps. No interpolation, polling-based temperature controller, guessed default curve, or CPU/GPU fan-label swap is introduced.

The native graph shows draggable finite points in temperature/speed coordinates, without connecting or hysteresis guide lines. Point movements are constrained by adjacent entries and validated against the cooling floor. Changing a rising threshold shifts its falling threshold to retain a nonnegative hysteresis gap; an existing overlapping factory pair stays intact when only duty changes. The graph is an editor view, not a simulation of the firmware's transition timing. Sentinel entries remain in the underlying curve and are not exposed as draggable points. Graph operations and keyboard edits update memory only; Apply curve is the separate driver operation.

The boost button reads the current mode byte and toggles only bit `0x40`, preserving every other bit. Its label is derived from subsequent hardware readback, rather than assuming a click changed the state.

Curve validation requires exactly 16 entries per fan, byte-representable temperatures, a maximum finite temperature of 110°C, nondecreasing rising thresholds/duties, and at least 80% duty at a reachable finite threshold no higher than 95°C. Sentinel `255` entries do not satisfy that cooling floor. New falling thresholds cannot exceed rising thresholds. An existing firmware pair with an overlap may remain unchanged: the inspected GPU factory curve includes up=70/down=71.

During programming, full fan boost is selected first. The complete table is written, the split/independent/table bits are enabled using read/modify/write, and the boost bit is cleared last. Final register values are checked after the batch. This stages a series of driver calls; the EC does not provide an atomic table transaction. Existing firmware safety behavior is not replaced by a software thermal loop.

## Recovery and concurrent changes

Before the first write, the application captures the old values and persists both old and intended values under `%LOCALAPPDATA%\NUCtoolRust\recovery.json`. The file is written and flushed to a temporary file, then replaced with a write-through Windows rename. If that fails, the write is refused. Windows power-plan GUIDs are also retained for preset restoration.

All EC work runs on one worker thread. The UI sends typed requests and receives reports/results; it does not hold the driver handle. The process-local queue and GUI single-instance check cannot lock out the manufacturer service or another driver client.

Before a batch, captured values must still match. Every final value must read back correctly. An error causes reverse-order rollback with readback. A rollback failure retains the recovery state and blocks further changes pending recovery. Updating an existing override keeps the first original value rather than replacing it with the previous override.

Normal window close requests restoration and waits for the worker's result before destroying the window. When another program has changed a group, automatic restoration leaves it alone and retains recovery data. Explicit Restore can replace the external state after the application's confirmation dialog. A preexisting interrupted session is never restored automatically at startup or exit without that explicit action.

The recovery file is a best-effort record, not a hardware watchdog. A driver that blocks indefinitely can block the worker; forced process termination or power loss can interrupt restoration. The installed service can reapply policy on AC/DC changes and resume. No background enforcement loop retries writes against it.

## Diagnostics and boundaries

`--diagnose` constructs inventory, reads EC sensors and the table, and queries NVML. It does not start the UI, save a configuration, apply a preset, or call write functions. The machine UUID used for recovery is omitted from the printed report. GPU readings include documented NVML power, clock, temperature, core-count, and limitation-reason queries; missing functions or unsupported queries remain unavailable.

The native app intentionally does not invoke the vendor's native overclocking exports, undocumented NVAPI setters, arbitrary PCI/memory IOCTLs, BIOS tools, or hidden-core/cache changes. See the [performance investigation](performance-investigation.md) for concrete findings and what would be needed to establish additional controls.
