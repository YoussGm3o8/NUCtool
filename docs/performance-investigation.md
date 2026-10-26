# Performance investigation: what is real on this LAPKC71E

Inspected October 26, 2026. This adds read-only native diagnostics and further binary/interface inspection to the [previous static Software Studio comparison](software-studio-comparison.md). No hardware settings, BIOS configuration, services, driver security settings, clock offsets, or voltages were changed. No performance gain was measured or claimed.

## Observed hardware and limits

| Item | Read-only observation | Interpretation |
| --- | --- | --- |
| CPU | i7-11800H, 8 cores, 16 logical processors, 24,576 KB L3. | Matches Intel's published 8-core/16-thread/24 MB configuration. The installed system is already exposing that configuration. |
| CPU EC limits | PL1 65 W, PL2 90 W. | These matched the firmware performance preset at `0x07A7/A8` during the sample. They are requests reported by the EC, not a direct measurement of package consumption. |
| GPU | RTX 3060 Laptop GPU, 3,840 CUDA cores reported by NVML. | This is what the installed driver exposes. No additional disabled units were discovered. |
| NVIDIA driver | 576.52. | Conclusions about supported operations are specific to the installed hardware/driver combination. |
| GPU power data | Default limit 80 W; constraint range 1–130 W; current power-limit query unsupported. | The range does **not** establish that this laptop accepts writes to that limit. The new app leaves GPU power controls read-only. |
| GPU clock data | `nvidia-smi` reported maximum graphics 2,100 MHz and memory 7,001 MHz; sample current clocks were much lower at idle. | A reported maximum is not a sustainable clock guarantee or an overclocking range. Idle clocks are not evidence of throttling. |
| GPU limitation data | Idle active in the sampled state. Historical driver counters also included software power capping and thermal slowdown time. | Counters have a history window; they do not identify the current workload's bottleneck or justify a higher power limit. The app shows current reason flags. |
| Services/drivers | UniwillService and UWACPIDriver running; no matching XTU, PerformanceDriver, WinRing, or RwDrv driver found in the bounded driver-name inventory. | Those additional interfaces were not available to inspect as installed providers in this audit. This is not a claim that all possible tuning tools were searched. |

The new Rust executable read both fan RPMs, current fan tables, temperatures, charge limit, CPU limits/presets, and the NVIDIA values above without the .NET/WCF bridge. Values such as temperatures and power draw vary continuously and are not benchmark results.

Intel's [i7-11800H specifications](https://www.intel.com/content/www/us/en/products/sku/213803/intel-core-i711800h-processor-24m-cache-up-to-4-60-ghz/specifications.html) confirm the core/thread/cache configuration and a maximum turbo frequency of 4.60 GHz. That maximum does not mean all cores sustain that speed simultaneously.

## Beyond changing power limits

Further inspection followed `UniwillManager.DeviceControlModel.NvidiaSetting` through its P/Invoke declarations in the obfuscated `_t52eCaacHXLcj2hqRMzhprtyNNf` class into the installed native `UWNvWrapper.dll`. Reading the native DLL's PE export table confirmed more than the service's high-level controls:

| Interface found | What it could support | What remains unestablished |
| --- | --- | --- |
| `GetNVCoreFreqOffset` / `SetNVCoreFreqOffset` | Core clock offset inspection/adjustment. | Complete structures, units, supported min/max range, GPU/driver acceptance, and restore behavior. |
| `SetNVCoreclock`, `SetNVGpuCoreLockFrequency` | Clock selection/locking paths. | Whether the value is an absolute clock, a target, or a driver-specific policy; applicability and interaction with boost. |
| `GetNVGpuVoltage` and `SetNVGpuVoltageDelta`, `SetNVGpuOvVoltageDelta`, `SetNVGpuLockVoltage` | Voltage-related read/adjustment paths. | Supported voltage ranges, mode semantics, firmware constraints, and safe rollback. An export name is not evidence an undervolt is accepted. |
| `GetNVPState`, `SetNVPState`, `EnableDynamicPstates` | GPU performance-state policy. | Which states/changes are accepted on this driver and whether a change helps the workload. |
| Service `SetDGpuTargetTemperature` | A temperature target routed to NVIDIA control code. | Accepted range and actual fan/clock/thermal-policy effect. |
| Service Whisper methods | Frame-rate/quiet-policy management where the service reports support. | Whether the current driver/installation supports the requested mode; policy may limit performance rather than raise it. |
| `PerfMode.SetEcOCConfigureTGP` / `SetEcOCDynamicBoost` | Manufacturer GPU budget/Dynamic Boost paths. | Correct parameter bounds and full policy sequence for an independent native implementation. These are not proof of permission or ability to exceed the board's limits. |

`UWNvWrapper.dll` is a native PE, not a managed assembly. ILSpy correctly reported that it has no managed metadata. Export names and managed call declarations do not recover all native function bodies or argument semantics. It was inspected, not loaded into the new application or called for tuning. The new executable does not redistribute or depend on it.

The manufacturer service's EC GPU path also uses registers around `0x0743..0x0746`, including flags and separate configured TGP/Dynamic Boost data. The decompiled Dynamic Boost method contains a write of 45 to `0x0745` and further writes to the surrounding registers, illustrating why copying a single byte write is insufficient. Those values are not offered as tuning instructions; the complete state transitions have not been established.

The direct Arc-labeled `0x072D/0x072E` GPU controls in old NUCtool were removed from the native edition. An NVIDIA laptop requires the correct NVIDIA/platform path, not the same labels on another register pair.

For public APIs, the new implementation uses NVIDIA's [NVML header](https://github.com/NVIDIA/go-nvml/blob/main/pkg/nvml/nvml.h) and [NVIDIA System Management Interface documentation](https://docs.nvidia.com/deploy/nvidia-smi/index.html). NVIDIA documents explicit unsupported-operation results and model-specific management capabilities. The application preserves that distinction rather than enabling a setter because a symbol exists.

## Manufacturer restrictions, CPU cores, and cache

There are several different restrictions to distinguish:

1. **Software UI restrictions:** the vendor UI can couple choices even when the service contract has separate fields. The earlier investigation found its profile editor assigning GPU mode from CPU mode. A new client could expose independent supported choices after testing them.
2. **Service/driver policy:** a function can reject a request or have a different contract between service versions. The old bundled client's `SetPowerPlan` action is absent from the installed service contract. The new app uses Windows' native power-plan API instead.
3. **Firmware/board policy:** the EC, BIOS, GPU VBIOS, cooling system, and power delivery can impose limits independent of a UI. Reading a numeric constraint does not prove it can be overridden or that it represents unused thermal capacity.
4. **Silicon configuration:** the hardware units exposed for this SKU are not an EC fan/power setting. Finding physical units beyond the marketed configuration would require specific evidence; none was found here.

The CPU's observed topology already matches the published SKU. There is no observed missing marketed core or cache allocation to restore. Neither the inspected service contract nor its named performance methods exposed a core/cache activation operation. This does not reconstruct silicon fuses or prove the physical die contains nothing disabled; it establishes that a hidden-core/cache “unlock” has no demonstrated implementation on this machine.

Intel's [XTU requirements](https://www.intel.com/content/www/us/en/support/articles/000006636/processors/processor-utilities-and-programs.html) describe support for unlocked processor configurations. The existence of the installed Uniwill EC driver does not establish a supported multiplier or voltage-control path for this i7-11800H. Intel's newer [Undervolt Protection article](https://www.intel.com/content/www/us/en/support/articles/000094219/processors.html) explicitly applies that named feature to 12th-generation and newer CPUs; it should not be used to diagnose this 11th-generation laptop's voltage restrictions.

The old service's `ACPICtrl` class also has PCI and memory-access helpers beyond EC byte access. Such broad access is not itself a tuning feature, and its presence does not identify an MSR interface, an overclocking lock, or a core/cache-enable mechanism. No arbitrary memory/PCI operations, security changes, or firmware patches were attempted.

## Concrete next candidates

The most credible non-power-limit candidate is a **bounded GPU clock/voltage control path**, because named native wrapper methods and service NVIDIA integration actually exist. It needs a recovered native ABI, documented or independently verified units/ranges, read-only capability queries, a captured restore state, and a controlled workload comparison before enabling setters in the app. None of those can be replaced by a button labeled “unlock.”

Other useful candidates are supported independent AC/DC performance policies, service-managed keyboard effects, and richer telemetry. A native client for the existing WCF service is possible in principle, but it would require its binary SOAP/WebSocket/session/callback protocol; implementing that stack was not necessary for the small direct-driver application.

First identify an actual bottleneck with a reproducible workload: thermal limitations, power capping, CPU/GPU utilization, clocks, and frame-time/performance measurements. Then change one established control and compare against the same baseline. No automated tuning or background benchmark was run during development of this rewrite.

## Reproduction boundaries

The earlier comparison records the hashes of the installed application/service and the exact decompiled symbol locators. Additional native export inspection used the installed `C:\Windows\System32\UniwillService\UWNvWrapper.dll`, SHA-256 `1262D46728041026AD49CE475FF9A84086A11CB388A8736AA7D5CFB88A234FBA`. The Rust diagnostic report can be reproduced with `nuctool.exe --diagnose`; it excludes the machine UUID and performs no hardware writes. The repository does not contain decompiled proprietary source, vendor DLLs, unsigned drivers, or a BIOS flashing tool.

The native release is deliberately limited to controls with an established EC layout and tested failure handling. It is not hardware-qualified for write operation yet, does not claim previously hidden hardware, and does not claim an increase in performance.
