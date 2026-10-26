use crate::model::{FanCurve, FanPoint, Result, FAN_BASES};
use std::{ffi::c_void, mem::size_of, ptr, thread, time::Duration};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::IO::DeviceIoControl,
};

pub trait Registers {
    fn read(&self, address: u16) -> Result<u8>;
    fn write(&self, address: u16, value: u8) -> Result<()>;
}

pub struct Ec(HANDLE);

impl Ec {
    pub fn open() -> Result<Self> {
        let path: Vec<u16> = "\\\\.\\ACPIDriver\0".encode_utf16().collect();
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!("ACPI driver access failed (Windows {}). Run as administrator; UWACPIDriver must be installed.", unsafe { GetLastError() }));
        }
        Ok(Self(handle))
    }
}

impl Registers for Ec {
    fn read(&self, address: u16) -> Result<u8> {
        let input = u32::from(address);
        let mut output = 0u32;
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.0,
                0x9C40A488,
                (&input as *const u32).cast(),
                4,
                (&mut output as *mut u32).cast(),
                4,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 || returned != size_of::<u32>() as u32 {
            return Err(format!(
                "Reading EC {address:#06X} failed (Windows {}, {returned} bytes).",
                unsafe { GetLastError() }
            ));
        }
        Ok((output & 255) as u8)
    }

    fn write(&self, address: u16, value: u8) -> Result<()> {
        let input = [u32::from(address), u32::from(value)];
        let mut output = 0u32;
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.0,
                0x9C40A48C,
                input.as_ptr().cast::<c_void>(),
                8,
                (&mut output as *mut u32).cast(),
                4,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(format!(
                "Writing EC {address:#06X} failed (Windows {}).",
                unsafe { GetLastError() }
            ));
        }
        thread::sleep(Duration::from_millis(6));
        Ok(())
    }
}

impl Drop for Ec {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub fn read_curve(ec: &impl Registers) -> Result<FanCurve> {
    let mut fans = Vec::new();
    for fan in 0..2 {
        let mut points = Vec::new();
        for i in 0..16 {
            points.push(FanPoint {
                up: ec.read(FAN_BASES[fan * 3] + i)?,
                down: ec.read(FAN_BASES[fan * 3 + 1] + i)?,
                duty: ec.read(FAN_BASES[fan * 3 + 2] + i)?,
            });
        }
        fans.push(points);
    }
    Ok(FanCurve {
        cpu: fans.remove(0),
        gpu: fans.remove(0),
    })
}

pub fn rpm(ec: &impl Registers, address: u16) -> Result<u16> {
    Ok(u16::from_be_bytes([
        ec.read(address)?,
        ec.read(address + 1)?,
    ]))
}
