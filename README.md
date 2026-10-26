# NUCtool

A small native Windows control panel, written in Rust. One executable; no WebView, JavaScript runtime, .NET bridge, bundled kernel driver, installer, or background service of its own.

This fork replaces the previous Tauri application. The original is preserved in [Git history at 9724689](https://github.com/cyear/NUCtool/tree/97246897e9b38a7a30af0d7abecb7445baa0118e). The project remains GPL-3.0; see [LICENSE.txt](LICENSE.txt).

**Hardware status:** read-only diagnostics have been verified on an Intel LAPKC71E with an i7-11800H and RTX 3060 Laptop GPU. Write behavior has automated tests using simulated registers; physical write, cooling/load, and sleep/resume qualification is still pending. EC write controls are restricted to that model/CPU/BIOS family. Other computers can display available inventory and NVIDIA telemetry.

## What it does

- Shows CPU/GPU temperatures, both fan RPMs, GPU power draw, clocks, CUDA-core count, and clock-limitation reasons. Missing readings appear as unavailable.
- Applies CPU firmware power presets or explicit PL1/PL2 values, up to the firmware performance preset. Presets also select the corresponding Windows power plan.
- Reads the actual 16-entry CPU/GPU firmware fan tables into a draggable graph, preserving independent rising/falling thresholds and half-percent duty precision. Supports full fan boost and saving/loading a curve.
- Sets a battery charge limit between 50 and 100%.
- Saves recovery data **before** a hardware write, verifies requested values, and rolls back failures. Normal window close restores the session's overrides. An interrupted session requires explicit recovery on the next start.

GPU performance controls remain read-only: the installed RTX driver's current power-limit interface reports unsupported even though it reports constraint values. The application does not pretend that another GPU's EC registers control this one.

## Run

Build with a Windows x64 MSVC Rust toolchain:

```powershell
cargo build --release --locked
.\target\release\nuctool.exe
```

Use **Run as administrator** when accessing the installed Uniwill ACPI driver. The application does not install that driver, restart manufacturer services, elevate itself, or apply settings at startup.

Read-only diagnostics, including the current firmware curves:

```powershell
.\target\release\nuctool.exe --diagnose
```

The four pages are Overview, CPU & battery, Fan curves, and Capabilities. Overview shows the detected machine's hardware and live readings. Select CPU fan or GPU fan, then drag a graph point horizontally for temperature and vertically for fan speed. A solid line connects the editable points; the dotted hysteresis guide is hidden. Dragging preserves the selected point's hysteresis gap where possible and prevents points crossing their neighbors.

The selected point also has precise temperature and speed fields. Left/Right selects a graph point, Up/Down adjusts duty by 0.5%, and Ctrl+Left/Right adjusts temperature by 1°C. Firmware sentinel entries are preserved internally and are not draggable. Read hardware and Load saved only edit the form. Apply curve is the separate hardware write action. New curves must reach at least 80% duty by 95°C before the sentinel entries. Full fan boost toggles on/off, with its button label reflecting the firmware state.

Configuration and interrupted-session recovery live in `%LOCALAPPDATA%\NUCtoolRust`. Saved curves are model-specific; recovery is tied to the machine UUID and BIOS version. A second GUI instance in the same Windows session is refused. On normal exit, settings changed by another program are left alone and retained in recovery data. The explicit Restore action can replace those external changes and asks first.

Manufacturer software can reapply settings when the power source changes or the computer resumes. NUCtool reports observed EC drift and does not continually fight the service. Forced termination, logout, power loss, or a blocked driver call can prevent automatic restoration; use the recovery action after restarting.

## Scope and research

This is a focused native rewrite, not feature parity with the previous application. BIOS NVRAM writing, single-color keyboard register guesses, the legacy software fan loop, service-dependent display/lighting controls, autostart, OSD, and automatic update checks are not included. The fan control is performed by firmware after the table is programmed.

- [Native backend and recovery design](docs/native-backend.md)
- [Performance investigation: manufacturer interfaces, real limits, cores and cache](docs/performance-investigation.md)
- [Historical NUCtool backend audit](docs/backend-hardware-control.md)
- [Installed Software Studio comparison](docs/software-studio-comparison.md)

No extra cores/cache, firmware bypass, or benchmark gain is claimed. The investigation distinguishes observed hardware, implemented interfaces, and unverified candidates.

## Development checks

```powershell
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Tests do not access real hardware. The diagnostic mode reads hardware but never calls an EC write or a power-plan setter. The release build uses the Windows GUI subsystem and attaches to the caller's console only for `--diagnose`.
