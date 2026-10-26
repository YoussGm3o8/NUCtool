use crate::{
    ec::{self, Ec, Registers},
    gpu::{Gpu, GpuSample},
    journal::{self, Change, Journal},
    machine::Machine,
    model::{self, FanCurve, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    ptr,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};
use windows_sys::{
    core::GUID,
    Win32::{
        Foundation::LocalFree,
        System::Power::{PowerGetActiveScheme, PowerSetActiveScheme},
    },
};

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub machine: Machine,
    pub cpu_c: Option<u8>,
    pub gpu_ec_c: Option<u8>,
    pub rpm: [Option<u16>; 2],
    pub cpu_limits: [Option<u8>; 2],
    pub cpu_ceiling: Option<(u8, u8)>,
    pub charge_limit: Option<u8>,
    pub fan_boost: Option<bool>,
    pub has_overrides: bool,
    pub gpu: Option<GpuSample>,
    pub gpu_error: Option<String>,
    pub ec_error: Option<String>,
    pub status: String,
    pub recovery_pending: bool,
    pub can_write: bool,
    pub curve: Option<FanCurve>,
}

pub enum Job {
    Cpu(u8, u8),
    Preset(usize),
    Curve(FanCurve),
    Boost,
    Battery(u8),
    ReadCurve,
    SaveCurve(FanCurve),
    LoadCurve,
    Restore,
    Quit,
}

pub enum Event {
    Report(Box<Report>),
    Message(Result<()>),
    Curve(FanCurve),
    Quit(Result<()>),
    Fatal(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedCurve {
    model: String,
    curve: FanCurve,
}

struct Backend {
    machine: Machine,
    ec: Result<Ec>,
    gpu: Result<Gpu>,
    journal: Result<Journal>,
    journal_path: Result<PathBuf>,
    recovery_pending: bool,
}

impl Backend {
    fn new() -> Result<Self> {
        let machine = Machine::read()?;
        let journal_path = journal::data_dir().map(|p| p.join("recovery.json"));
        let journal = journal_path
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|p| Journal::load(p, &machine.identity));
        let recovery_pending = journal
            .as_ref()
            .is_ok_and(|j| !j.groups.is_empty() || j.scheme.is_some());
        let ec = if machine.supports_ec() {
            Ec::open()
        } else {
            Err(format!("No validated EC layout is available for {} (BIOS {}). Hardware writes are disabled.", machine.model, machine.bios))
        };
        Ok(Self {
            machine,
            ec,
            gpu: Gpu::open(),
            journal,
            journal_path,
            recovery_pending,
        })
    }

    fn ceiling(&self) -> Result<(u8, u8)> {
        let ec = self.ec.as_ref().map_err(Clone::clone)?;
        let values = (ec.read(0x07A7)?, ec.read(0x07A8)?);
        if values.0 == 0 || values.1 < values.0 {
            return Err(
                "The firmware performance preset could not be validated. CPU writes are disabled."
                    .into(),
            );
        }
        Ok(values)
    }

    fn report(&self, include_curve: bool) -> Report {
        let read = |address| self.ec.as_ref().ok().and_then(|ec| ec.read(address).ok());
        let mut report = Report {
            machine: self.machine.clone(),
            cpu_c: read(0x043E),
            gpu_ec_c: read(0x044F),
            rpm: [0x0464, 0x046C].map(|a| self.ec.as_ref().ok().and_then(|ec| ec::rpm(ec, a).ok())),
            cpu_limits: [read(0x0783), read(0x0784)],
            cpu_ceiling: self.ceiling().ok(),
            charge_limit: read(0x07B9).map(|v| v & 127),
            fan_boost: read(0x0751).map(|value| value & 0x40 != 0),
            has_overrides: self
                .journal
                .as_ref()
                .is_ok_and(|j| !j.groups.is_empty() || j.scheme.is_some()),
            gpu: self.gpu.as_ref().ok().map(Gpu::sample),
            gpu_error: self.gpu.as_ref().err().cloned(),
            ec_error: self.ec.as_ref().err().cloned(),
            status: String::new(),
            recovery_pending: self.recovery_pending,
            can_write: self.ec.is_ok() && self.journal.is_ok() && !self.recovery_pending,
            curve: if include_curve {
                self.ec.as_ref().ok().and_then(|ec| ec::read_curve(ec).ok())
            } else {
                None
            },
        };
        report.status = match &self.journal {
            Err(e) => e.clone(),
            Ok(_) if self.recovery_pending => {
                "Restore the previous session before applying changes.".into()
            }
            Ok(journal) if journal.groups.is_empty() && journal.scheme.is_none() => String::new(),
            Ok(journal) => {
                let drift = self.ec.as_ref().ok().is_some_and(|ec| {
                    journal
                        .groups
                        .iter()
                        .flat_map(|g| &g.changes)
                        .any(|c| ec.read(c.address).ok() != Some(c.after))
                });
                if drift {
                    "A setting changed outside NUCtool. Overrides will not be reasserted; review before restoring.".into()
                } else {
                    String::new()
                }
            }
        };
        report
    }

    fn apply(&mut self, name: &str, writes: Vec<(u16, u8)>) -> Result<()> {
        if self.recovery_pending {
            return Err("Restore the previous session before applying changes.".into());
        }
        let ec = self.ec.as_ref().map_err(Clone::clone)?;
        let old = self.journal.as_ref().map_err(Clone::clone)?;
        let path = self.journal_path.as_ref().map_err(Clone::clone)?;
        let (next, changes) = old.prepare(ec, name, &writes)?;
        journal::save_json(path, &next)?;
        match journal::transact(ec, &changes) {
            Ok(()) => {
                self.journal = Ok(next);
                Ok(())
            }
            Err(e) => {
                if e.rollback_ok {
                    if let Err(save_error) = journal::save_json(path, old) {
                        self.journal = Ok(next);
                        self.recovery_pending = true;
                        return Err(format!(
                            "{} Recovery journal could not be reverted: {save_error}",
                            e.message
                        ));
                    }
                } else {
                    self.journal = Ok(next);
                    self.recovery_pending = true;
                }
                Err(e.message)
            }
        }
    }

    fn restore(&mut self, explicit: bool) -> Result<()> {
        if self.recovery_pending && !explicit {
            return Ok(());
        }
        let journal = self.journal.as_mut().map_err(|e| e.clone())?;
        if journal.groups.is_empty() {
            return self.restore_scheme(explicit);
        }
        let ec = self.ec.as_ref().map_err(Clone::clone)?;
        let path = self.journal_path.as_ref().map_err(Clone::clone)?;
        let mut errors = Vec::new();
        let mut remaining = Vec::new();
        for group in journal.groups.iter().rev() {
            let result = (|| -> Result<()> {
                let mut changes = Vec::new();
                for c in group.changes.iter().rev() {
                    let current = ec.read(c.address)?;
                    if !explicit && current != c.after && current != c.before {
                        return Err(format!(
                            "{} changed outside NUCtool; its recovery data was kept.",
                            group.name
                        ));
                    }
                    if current != c.before {
                        changes.push(Change {
                            address: c.address,
                            before: current,
                            after: c.before,
                        });
                    }
                }
                if group.name == "CPU" {
                    let target = |address| {
                        group
                            .changes
                            .iter()
                            .find(|c| c.address == address)
                            .map(|c| c.before)
                            .ok_or("Incomplete CPU recovery data.")
                    };
                    let order =
                        model::cpu_writes(ec.read(0x0784)?, target(0x0783)?, target(0x0784)?);
                    changes.sort_by_key(|c| order.iter().position(|(a, _)| *a == c.address));
                }
                if group.name == "Fans"
                    && !changes.is_empty()
                    && group
                        .changes
                        .iter()
                        .any(|c| (0x0F00..0x0F60).contains(&c.address))
                {
                    let mode = ec.read(0x0751)?;
                    let original = group
                        .changes
                        .iter()
                        .find(|c| c.address == 0x0751)
                        .ok_or("Incomplete fan recovery data.")?
                        .before;
                    changes.retain(|c| c.address != 0x0751);
                    changes.insert(
                        0,
                        Change {
                            address: 0x0751,
                            before: mode,
                            after: mode | 0x40,
                        },
                    );
                    changes.push(Change {
                        address: 0x0751,
                        before: mode,
                        after: original,
                    });
                }
                journal::transact(ec, &changes).map_err(|e| e.message)
            })();
            if let Err(e) = result {
                errors.push(e);
                remaining.push(group.clone());
            }
        }
        remaining.reverse();
        journal.groups = remaining;
        journal::save_json(path, journal)?;
        self.recovery_pending = !journal.groups.is_empty() || journal.scheme.is_some();
        if let Err(e) = self.restore_scheme(explicit) {
            errors.push(e);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }

    fn restore_scheme(&mut self, explicit: bool) -> Result<()> {
        let journal = self.journal.as_mut().map_err(|e| e.clone())?;
        if let Some([before, expected]) = journal.scheme {
            let current = active_scheme()?;
            if !explicit
                && !same_guid(&current, &GUID::from_u128(expected))
                && !same_guid(&current, &GUID::from_u128(before))
            {
                return Err(
                    "The Windows power plan changed outside NUCtool; it was left in place.".into(),
                );
            }
            set_scheme(&GUID::from_u128(before))?;
            journal.scheme = None;
            journal::save_json(self.journal_path.as_ref().map_err(Clone::clone)?, journal)?;
        }
        self.recovery_pending = !journal.groups.is_empty();
        Ok(())
    }

    fn execute(&mut self, job: Job) -> Result<Option<FanCurve>> {
        match job {
            Job::Cpu(pl1, pl2) => {
                model::validate_cpu(pl1, pl2, self.ceiling()?)?;
                let current_pl2 = self.ec.as_ref().map_err(Clone::clone)?.read(0x0784)?;
                self.apply("CPU", model::cpu_writes(current_pl2, pl1, pl2))?;
            }
            Job::Preset(index) => {
                let ec = self.ec.as_ref().map_err(Clone::clone)?;
                let base = *[0x0734, 0x0730, 0x07A7]
                    .get(index)
                    .ok_or("Unknown preset.")?;
                let (pl1, pl2) = (ec.read(base)?, ec.read(base + 1)?);
                model::validate_cpu(pl1, pl2, self.ceiling()?)?;
                let original = active_scheme()?;
                let current_pl2 = ec.read(0x0784)?;
                self.apply("CPU", model::cpu_writes(current_pl2, pl1, pl2))?;
                let scheme = [
                    GUID::from_u128(0xa1841308_3541_4fab_bc81_f71556f20b4a),
                    GUID::from_u128(0x381b4222_f694_41f0_9685_ff5bb260df2e),
                    GUID::from_u128(0x8c5e7fda_e8bf_4a96_9a85_a6e23a8c635c),
                ][index];
                let journal = self.journal.as_mut().map_err(|e| e.clone())?;
                let before = journal
                    .scheme
                    .map(|values| values[0])
                    .unwrap_or_else(|| guid_value(&original));
                journal.scheme = Some([before, guid_value(&scheme)]);
                journal::save_json(self.journal_path.as_ref().map_err(Clone::clone)?, journal)?;
                set_scheme(&scheme)?;
            }
            Job::Curve(curve) => {
                let ec = self.ec.as_ref().map_err(Clone::clone)?;
                curve.validate_against(&ec::read_curve(ec)?)?;
                let data = curve.writes()?;
                let mode = ec.read(0x0751)?;
                let mut writes = vec![(0x0751, mode | 0x40)];
                writes.extend(data);
                // Keep full fan boost during programming, then enable the completed table.
                writes.extend([
                    (0x0741, ec.read(0x0741)? | 1),
                    (0x07C5, ec.read(0x07C5)? | 0x80),
                    (0x07C6, ec.read(0x07C6)? | 4),
                    (0x0751, mode & !0x40),
                ]);
                self.apply("Fans", writes)?;
            }
            Job::Boost => {
                let ec = self.ec.as_ref().map_err(Clone::clone)?;
                self.apply(
                    "Fans",
                    vec![(0x0751, model::toggle_boost(ec.read(0x0751)?))],
                )?;
            }
            Job::Battery(limit) => {
                if !(50..=100).contains(&limit) {
                    return Err("Choose a battery charge limit between 50 and 100%.".into());
                }
                let ec = self.ec.as_ref().map_err(Clone::clone)?;
                self.apply("Battery", vec![(0x07B9, (ec.read(0x07B9)? & 0x80) | limit)])?;
            }
            Job::ReadCurve => {
                return Ok(Some(ec::read_curve(
                    self.ec.as_ref().map_err(Clone::clone)?,
                )?))
            }
            Job::SaveCurve(curve) => {
                curve.validate()?;
                journal::save_json(
                    &journal::data_dir()?.join("fan-curve.json"),
                    &SavedCurve {
                        model: self.machine.model.clone(),
                        curve,
                    },
                )?;
            }
            Job::LoadCurve => {
                let bytes = fs::read(journal::data_dir()?.join("fan-curve.json"))
                    .map_err(|e| e.to_string())?;
                let saved: SavedCurve =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                if saved.model != self.machine.model {
                    return Err("Saved curve belongs to another model.".into());
                }
                saved.curve.validate()?;
                return Ok(Some(saved.curve));
            }
            Job::Restore => self.restore(true)?,
            Job::Quit => unreachable!(),
        }
        Ok(None)
    }
}

fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
fn guid_value(guid: &GUID) -> u128 {
    ((guid.data1 as u128) << 96)
        | ((guid.data2 as u128) << 80)
        | ((guid.data3 as u128) << 64)
        | u64::from_be_bytes(guid.data4) as u128
}

fn active_scheme() -> Result<GUID> {
    let mut pointer = ptr::null_mut();
    let code = unsafe { PowerGetActiveScheme(ptr::null_mut(), &mut pointer) };
    if code != 0 || pointer.is_null() {
        return Err(format!("Cannot read Windows power plan ({code})."));
    }
    let guid = unsafe { *pointer };
    unsafe {
        LocalFree(pointer.cast());
    }
    Ok(guid)
}

fn set_scheme(guid: &GUID) -> Result<()> {
    let code = unsafe { PowerSetActiveScheme(ptr::null_mut(), guid) };
    if code != 0 {
        return Err(format!("Windows rejected the power plan ({code}). CPU overrides remain listed for restoration."));
    }
    if !same_guid(&active_scheme()?, guid) {
        return Err("Windows power plan readback did not match.".into());
    }
    Ok(())
}

pub fn diagnose() -> Result<Report> {
    Ok(Backend::new()?.report(true))
}

pub fn start() -> (Sender<Job>, Receiver<Event>) {
    let (jobs_tx, jobs_rx) = mpsc::channel();
    let (events_tx, events_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut backend = match Backend::new() {
            Ok(b) => b,
            Err(e) => {
                let _ = events_tx.send(Event::Fatal(e));
                return;
            }
        };
        let _ = events_tx.send(Event::Report(Box::new(backend.report(true))));
        loop {
            match jobs_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(Job::Quit) => {
                    let _ = events_tx.send(Event::Quit(backend.restore(false)));
                    break;
                }
                Ok(job) => match backend.execute(job) {
                    Ok(Some(curve)) => {
                        let _ = events_tx.send(Event::Curve(curve));
                    }
                    result => {
                        let _ = events_tx.send(Event::Message(result.map(|_| ())));
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = backend.restore(false);
                    break;
                }
            }
            if events_tx
                .send(Event::Report(Box::new(backend.report(false))))
                .is_err()
            {
                let _ = backend.restore(false);
                break;
            }
        }
    });
    (jobs_tx, events_rx)
}
