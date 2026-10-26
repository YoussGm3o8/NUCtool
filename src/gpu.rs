use crate::model::Result;
use serde::Serialize;
use std::{
    ffi::{c_char, c_void, CStr},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HMODULE},
    System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32},
};

type Device = *mut c_void;
type Status = i32;
type GetUint = unsafe extern "C" fn(Device, *mut u32) -> Status;

#[derive(Clone, Debug, Default, Serialize)]
pub struct GpuSample {
    pub name: String,
    pub temperature_c: Option<u32>,
    pub power_mw: Option<u32>,
    pub power_limit_mw: Option<u32>,
    pub default_limit_mw: Option<u32>,
    pub power_range_mw: Option<(u32, u32)>,
    pub graphics_mhz: Option<u32>,
    pub memory_mhz: Option<u32>,
    pub cuda_cores: Option<u32>,
    pub clock_event_reasons: Option<u64>,
}

pub struct Gpu {
    library: HMODULE,
    device: Device,
    initialized: bool,
}

impl Gpu {
    pub fn open() -> Result<Self> {
        let name: Vec<u16> = "nvml.dll\0".encode_utf16().collect();
        let library =
            unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if library.is_null() {
            return Err("NVIDIA's NVML driver library is unavailable.".into());
        }
        let mut gpu = Self {
            library,
            device: ptr::null_mut(),
            initialized: false,
        };
        let init: unsafe extern "C" fn() -> Status = gpu.symbol(b"nvmlInit_v2\0")?;
        if unsafe { init() } != 0 {
            return Err("NVML initialization failed.".into());
        }
        gpu.initialized = true;
        let get: unsafe extern "C" fn(u32, *mut Device) -> Status =
            gpu.symbol(b"nvmlDeviceGetHandleByIndex_v2\0")?;
        if unsafe { get(0, &mut gpu.device) } != 0 {
            return Err("No NVIDIA GPU is available.".into());
        }
        Ok(gpu)
    }

    fn symbol<T: Copy>(&self, name: &[u8]) -> Result<T> {
        // All names/signatures come from NVIDIA's public NVML header; never load a local DLL.
        let address = unsafe { GetProcAddress(self.library, name.as_ptr()) }.ok_or_else(|| {
            format!(
                "NVML function {} is unavailable.",
                String::from_utf8_lossy(name)
            )
        })?;
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&address));
        Ok(unsafe { std::mem::transmute_copy(&address) })
    }

    fn uint(&self, name: &[u8]) -> Option<u32> {
        let function: GetUint = self.symbol(name).ok()?;
        let mut value = 0;
        (unsafe { function(self.device, &mut value) } == 0).then_some(value)
    }

    fn clock(&self, domain: u32) -> Option<u32> {
        let function: unsafe extern "C" fn(Device, u32, *mut u32) -> Status =
            self.symbol(b"nvmlDeviceGetClockInfo\0").ok()?;
        let mut value = 0;
        (unsafe { function(self.device, domain, &mut value) } == 0).then_some(value)
    }

    pub fn sample(&self) -> GpuSample {
        let mut name = [0 as c_char; 128];
        let name_fn: Option<unsafe extern "C" fn(Device, *mut c_char, u32) -> Status> =
            self.symbol(b"nvmlDeviceGetName\0").ok();
        let label = if name_fn
            .is_some_and(|f| unsafe { f(self.device, name.as_mut_ptr(), name.len() as u32) } == 0)
        {
            name[127] = 0;
            unsafe { CStr::from_ptr(name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        } else {
            "NVIDIA GPU".into()
        };
        let temp_fn: Option<unsafe extern "C" fn(Device, u32, *mut u32) -> Status> =
            self.symbol(b"nvmlDeviceGetTemperature\0").ok();
        let mut temperature = 0;
        let temperature_c = temp_fn.and_then(|f| {
            (unsafe { f(self.device, 0, &mut temperature) } == 0).then_some(temperature)
        });
        let range_fn: Option<unsafe extern "C" fn(Device, *mut u32, *mut u32) -> Status> = self
            .symbol(b"nvmlDeviceGetPowerManagementLimitConstraints\0")
            .ok();
        let (mut min, mut max) = (0, 0);
        let range = range_fn.and_then(|f| {
            (unsafe { f(self.device, &mut min, &mut max) } == 0).then_some((min, max))
        });
        let reason_fn: Option<unsafe extern "C" fn(Device, *mut u64) -> Status> = self
            .symbol(b"nvmlDeviceGetCurrentClocksEventReasons\0")
            .or_else(|_| self.symbol(b"nvmlDeviceGetCurrentClocksThrottleReasons\0"))
            .ok();
        let mut reasons = 0;
        GpuSample {
            name: label,
            temperature_c,
            power_mw: self.uint(b"nvmlDeviceGetPowerUsage\0"),
            power_limit_mw: self.uint(b"nvmlDeviceGetPowerManagementLimit\0"),
            default_limit_mw: self.uint(b"nvmlDeviceGetPowerManagementDefaultLimit\0"),
            power_range_mw: range,
            graphics_mhz: self.clock(0),
            memory_mhz: self.clock(2),
            cuda_cores: self.uint(b"nvmlDeviceGetNumGpuCores\0"),
            clock_event_reasons: reason_fn
                .and_then(|f| (unsafe { f(self.device, &mut reasons) } == 0).then_some(reasons)),
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        if self.initialized {
            if let Ok(shutdown) = self.symbol::<unsafe extern "C" fn() -> Status>(b"nvmlShutdown\0")
            {
                unsafe {
                    shutdown();
                }
            }
        }
        unsafe {
            FreeLibrary(self.library);
        }
    }
}

pub fn clock_reasons(bits: u64) -> String {
    let reasons: Vec<_> = [
        (1, "idle"),
        (2, "application clocks"),
        (4, "software power cap"),
        (8, "hardware slowdown"),
        (16, "sync boost"),
        (32, "software thermal limit"),
        (64, "hardware thermal limit"),
        (128, "power brake"),
        (256, "display clocks"),
    ]
    .into_iter()
    .filter_map(|(mask, label)| (bits & mask != 0).then_some(label))
    .collect();
    if reasons.is_empty() {
        if bits == 0 {
            "none reported".into()
        } else {
            format!("driver flags {bits:#x}")
        }
    } else {
        reasons.join(", ")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn thermal_and_idle_reasons_are_not_confused() {
        assert_eq!(super::clock_reasons(1), "idle");
        assert_eq!(
            super::clock_reasons(32 | 64),
            "software thermal limit, hardware thermal limit"
        );
        assert_eq!(super::clock_reasons(0), "none reported");
    }
}
