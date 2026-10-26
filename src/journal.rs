use crate::{
    ec::Registers,
    model::{Result, FAN_BASES, FAN_FLAGS},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub address: u16,
    pub before: u8,
    pub after: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub name: String,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub machine: String,
    pub groups: Vec<Group>,
    pub scheme: Option<[u128; 2]>,
}

pub fn data_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("LOCALAPPDATA")
            .ok_or("LOCALAPPDATA is unavailable; changes are disabled.")?,
    )
    .join("NUCtoolRust"))
}

pub fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or("No configuration directory.")?)
        .map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("tmp");
    let mut file = fs::File::create(&temporary).map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    drop(file);
    use std::os::windows::ffi::OsStrExt;
    let from: Vec<_> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(format!(
            "Cannot save recovery data: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

impl Journal {
    pub fn load(path: &Path, machine: &str) -> Result<Self> {
        let journal = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<Self>(&bytes)
                .map_err(|e| format!("Recovery file is invalid: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self {
                machine: machine.into(),
                groups: Vec::new(),
                scheme: None,
            },
            Err(e) => return Err(e.to_string()),
        };
        journal.validate(machine)?;
        Ok(journal)
    }

    pub fn validate(&self, machine: &str) -> Result<()> {
        if self.machine != machine || self.groups.len() > 3 {
            return Err("Recovery data belongs to a different machine/BIOS or is invalid. Writes are disabled.".into());
        }
        let mut names = HashSet::new();
        for group in &self.groups {
            let mut addresses = HashSet::new();
            if !names.insert(&group.name) || group.changes.is_empty() || group.changes.len() > 100 {
                return Err("Invalid recovery group.".into());
            }
            for change in &group.changes {
                let allowed = match group.name.as_str() {
                    "CPU" => [0x0783, 0x0784].contains(&change.address),
                    "Battery" => change.address == 0x07B9,
                    "Fans" => {
                        FAN_FLAGS.contains(&change.address)
                            || FAN_BASES
                                .iter()
                                .any(|base| (*base..*base + 16).contains(&change.address))
                    }
                    _ => false,
                };
                if !allowed || !addresses.insert(change.address) {
                    return Err(
                        "Recovery data contains an unsupported or duplicate register.".into(),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn prepare(
        &self,
        ec: &impl Registers,
        name: &str,
        writes: &[(u16, u8)],
    ) -> Result<(Self, Vec<Change>)> {
        if let Some(group) = self.groups.iter().find(|g| g.name == name) {
            for c in &group.changes {
                if ec.read(c.address)? != c.after {
                    return Err(format!("{name} changed outside NUCtool. Restore or restart before applying another override."));
                }
            }
        }
        let changes = writes
            .iter()
            .map(|(address, after)| {
                Ok(Change {
                    address: *address,
                    before: ec.read(*address)?,
                    after: *after,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut next = self.clone();
        let index = next
            .groups
            .iter()
            .position(|g| g.name == name)
            .unwrap_or_else(|| {
                next.groups.push(Group {
                    name: name.into(),
                    changes: Vec::new(),
                });
                next.groups.len() - 1
            });
        for c in &changes {
            if let Some(existing) = next.groups[index]
                .changes
                .iter_mut()
                .find(|old| old.address == c.address)
            {
                existing.after = c.after;
            } else {
                next.groups[index].changes.push(c.clone());
            }
        }
        next.validate(&self.machine)?;
        Ok((next, changes))
    }
}

pub struct WriteFailure {
    pub message: String,
    pub rollback_ok: bool,
}

pub fn transact(ec: &impl Registers, changes: &[Change]) -> std::result::Result<(), WriteFailure> {
    let apply = || -> Result<()> {
        for c in changes {
            ec.write(c.address, c.after)?;
        }
        for (i, c) in changes.iter().enumerate() {
            if changes[i + 1..]
                .iter()
                .any(|later| later.address == c.address)
            {
                continue;
            }
            if ec.read(c.address)? != c.after {
                return Err(format!(
                    "EC {:#06X} did not keep the requested value.",
                    c.address
                ));
            }
        }
        Ok(())
    };
    // Do the stale-state check separately: it must never cause rollback writes.
    for c in changes {
        match ec.read(c.address) {
            Ok(value) if value == c.before => (),
            Ok(_) => {
                return Err(WriteFailure {
                    message: "Settings changed before writing. Nothing was written.".into(),
                    rollback_ok: true,
                })
            }
            Err(e) => {
                return Err(WriteFailure {
                    message: e,
                    rollback_ok: true,
                })
            }
        }
    }
    if let Err(error) = apply() {
        let mut failures = Vec::new();
        let fan_mode = changes.first().filter(|c| {
            c.address == 0x0751 && changes.iter().filter(|c| c.address == 0x0751).count() > 1
        });
        if let Some(mode) = fan_mode {
            if let Err(e) = ec.write(0x0751, mode.before | 0x40) {
                failures.push(e);
            }
        }
        for c in changes.iter().rev() {
            if c.address == 0x0751 && fan_mode.is_some() {
                continue;
            }
            if let Err(e) = ec.write(c.address, c.before).and_then(|_| {
                if ec.read(c.address)? == c.before {
                    Ok(())
                } else {
                    Err(format!("Rollback mismatch at {:#06X}.", c.address))
                }
            }) {
                failures.push(e);
            }
        }
        if let Some(mode) = fan_mode {
            if let Err(e) = ec.write(0x0751, mode.before).and_then(|_| {
                if ec.read(0x0751)? == mode.before {
                    Ok(())
                } else {
                    Err("Fan mode rollback did not match.".into())
                }
            }) {
                failures.push(e);
            }
        }
        return Err(WriteFailure {
            message: if failures.is_empty() {
                format!("{error} Previous values restored.")
            } else {
                format!("{error} Recovery needed: {}", failures.join("; "))
            },
            rollback_ok: failures.is_empty(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    struct Fake {
        values: RefCell<[u8; 2]>,
        fail_once: Cell<bool>,
        writes: Cell<usize>,
    }
    impl Registers for Fake {
        fn read(&self, address: u16) -> Result<u8> {
            let index = match address {
                0 | 0x0783 => 0,
                1 | 0x0784 => 1,
                _ => panic!("unexpected test address"),
            };
            Ok(self.values.borrow()[index])
        }
        fn write(&self, address: u16, value: u8) -> Result<()> {
            self.writes.set(self.writes.get() + 1);
            let index = match address {
                0 | 0x0783 => 0,
                1 | 0x0784 => 1,
                _ => panic!("unexpected test address"),
            };
            self.values.borrow_mut()[index] = value;
            if address == 1 && self.fail_once.replace(false) {
                Err("simulated partial write".into())
            } else {
                Ok(())
            }
        }
    }
    fn changes() -> Vec<Change> {
        vec![
            Change {
                address: 0,
                before: 10,
                after: 20,
            },
            Change {
                address: 1,
                before: 30,
                after: 40,
            },
        ]
    }
    #[test]
    fn successful_transaction_is_read_back() {
        let ec = Fake {
            values: RefCell::new([10, 30]),
            fail_once: Cell::new(false),
            writes: Cell::new(0),
        };
        assert!(transact(&ec, &changes()).is_ok());
        assert_eq!(*ec.values.borrow(), [20, 40]);
    }
    #[test]
    fn even_a_failed_write_that_changed_hardware_is_rolled_back() {
        let ec = Fake {
            values: RefCell::new([10, 30]),
            fail_once: Cell::new(true),
            writes: Cell::new(0),
        };
        let error = transact(&ec, &changes()).err().unwrap();
        assert!(error.rollback_ok);
        assert_eq!(*ec.values.borrow(), [10, 30]);
    }
    #[test]
    fn external_change_is_not_overwritten() {
        let ec = Fake {
            values: RefCell::new([11, 30]),
            fail_once: Cell::new(false),
            writes: Cell::new(0),
        };
        assert!(transact(&ec, &changes()).is_err());
        assert_eq!(ec.writes.get(), 0);
    }
    #[test]
    fn recovery_is_limited_to_known_registers_on_the_same_machine() {
        let journal = Journal {
            machine: "machine".into(),
            scheme: None,
            groups: vec![Group {
                name: "CPU".into(),
                changes: vec![Change {
                    address: 0x0783,
                    before: 45,
                    after: 50,
                }],
            }],
        };
        assert!(journal.validate("machine").is_ok());
        assert!(journal.validate("other").is_err());
        let mut invalid = journal;
        invalid.groups[0].changes[0].address = 0xFFFF;
        assert!(invalid.validate("machine").is_err());
    }
    #[test]
    fn a_staged_register_is_verified_against_its_final_value() {
        let ec = Fake {
            values: RefCell::new([10, 30]),
            fail_once: Cell::new(false),
            writes: Cell::new(0),
        };
        let mut staged = changes();
        staged.push(Change {
            address: 0,
            before: 10,
            after: 25,
        });
        assert!(transact(&ec, &staged).is_ok());
        assert_eq!(*ec.values.borrow(), [25, 40]);
    }
    #[test]
    fn repeated_overrides_keep_the_original_recovery_value() {
        let ec = Fake {
            values: RefCell::new([45, 90]),
            fail_once: Cell::new(false),
            writes: Cell::new(0),
        };
        let journal = Journal {
            machine: "machine".into(),
            groups: Vec::new(),
            scheme: None,
        };
        let (first, writes) = journal.prepare(&ec, "CPU", &[(0x0783, 50)]).unwrap();
        assert!(transact(&ec, &writes).is_ok());
        let (second, _) = first.prepare(&ec, "CPU", &[(0x0783, 55)]).unwrap();
        assert_eq!(second.groups[0].changes[0].before, 45);
        assert_eq!(second.groups[0].changes[0].after, 55);
        ec.values.borrow_mut()[0] = 60;
        assert!(first.prepare(&ec, "CPU", &[(0x0783, 65)]).is_err());
    }
    #[test]
    fn successful_driver_return_without_readback_is_rejected() {
        struct IgnoredWrite;
        impl Registers for IgnoredWrite {
            fn read(&self, _: u16) -> Result<u8> {
                Ok(10)
            }
            fn write(&self, _: u16, _: u8) -> Result<()> {
                Ok(())
            }
        }
        let failure = transact(&IgnoredWrite, &changes()[..1]).err().unwrap();
        assert!(failure.rollback_ok);
        assert!(failure.message.contains("did not keep"));
    }
    #[test]
    fn rollback_failure_is_reported_for_recovery() {
        struct FailedDriver;
        impl Registers for FailedDriver {
            fn read(&self, _: u16) -> Result<u8> {
                Ok(10)
            }
            fn write(&self, _: u16, _: u8) -> Result<()> {
                Err("driver failed".into())
            }
        }
        let failure = transact(&FailedDriver, &changes()[..1]).err().unwrap();
        assert!(!failure.rollback_ok);
        assert!(failure.message.contains("Recovery needed"));
    }
    #[test]
    fn recovery_guids_roundtrip_without_truncation() {
        let journal = Journal {
            machine: "machine".into(),
            groups: Vec::new(),
            scheme: Some([u128::MAX, 0x381b4222_f694_41f0_9685_ff5bb260df2e]),
        };
        let bytes = serde_json::to_vec(&journal).unwrap();
        let loaded: Journal = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.scheme, journal.scheme);
    }
    #[test]
    fn fan_rollback_keeps_boost_until_the_table_is_restored() {
        use std::collections::BTreeMap;
        struct Fan {
            values: RefCell<BTreeMap<u16, u8>>,
            trace: RefCell<Vec<(u16, u8)>>,
            fail: Cell<bool>,
        }
        impl Registers for Fan {
            fn read(&self, address: u16) -> Result<u8> {
                Ok(self.values.borrow()[&address])
            }
            fn write(&self, address: u16, value: u8) -> Result<()> {
                self.trace.borrow_mut().push((address, value));
                self.values.borrow_mut().insert(address, value);
                if address == 0x0F00 && self.fail.replace(false) {
                    Err("partial table write".into())
                } else {
                    Ok(())
                }
            }
        }
        let fan = Fan {
            values: RefCell::new(BTreeMap::from([(0x0751, 0), (0x0F00, 30)])),
            trace: RefCell::new(Vec::new()),
            fail: Cell::new(true),
        };
        let changes = [
            Change {
                address: 0x0751,
                before: 0,
                after: 64,
            },
            Change {
                address: 0x0F00,
                before: 30,
                after: 40,
            },
            Change {
                address: 0x0751,
                before: 0,
                after: 0,
            },
        ];
        assert!(transact(&fan, &changes).err().unwrap().rollback_ok);
        assert_eq!(
            *fan.trace.borrow(),
            vec![
                (0x0751, 64),
                (0x0F00, 40),
                (0x0751, 64),
                (0x0F00, 30),
                (0x0751, 0)
            ]
        );
    }
}
