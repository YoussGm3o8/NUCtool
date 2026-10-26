use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, String>;

pub const FAN_BASES: [u16; 6] = [0x0F00, 0x0F10, 0x0F20, 0x0F30, 0x0F40, 0x0F50];
pub const FAN_FLAGS: [u16; 4] = [0x0751, 0x0741, 0x07C5, 0x07C6];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FanPoint {
    pub up: u8,
    pub down: u8,
    pub duty: u8,
}

impl FanPoint {
    pub fn adjusted(&self, up: u8, duty: u8) -> Self {
        let down = if up == self.up {
            self.down
        } else {
            up.saturating_sub(self.up.saturating_sub(self.down))
        };
        Self { up, down, duty }
    }
}

pub fn toggle_boost(mode: u8) -> u8 {
    mode ^ 0x40
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FanCurve {
    pub cpu: Vec<FanPoint>,
    pub gpu: Vec<FanPoint>,
}

impl FanCurve {
    pub fn validate(&self) -> Result<()> {
        for (name, points) in [("CPU", &self.cpu), ("GPU", &self.gpu)] {
            if points.len() != 16 {
                return Err(format!("{name}: exactly 16 firmware entries are required."));
            }
            for (i, p) in points.iter().enumerate() {
                if p.duty > 200 || (p.down > 110 && p.down != 255) {
                    return Err(format!("{name} entry {i}: duty must be 0–100%; falling temperature must be <= 110 or the 255 sentinel."));
                }
                if p.up > 110 && p.up != 255 {
                    return Err(format!("{name} entry {i}: use a temperature <= 110 or the firmware's 255 sentinel."));
                }
                if i > 0 && (p.up < points[i - 1].up || p.duty < points[i - 1].duty) {
                    return Err(format!(
                        "{name}: rising temperatures and fan duties must not decrease."
                    ));
                }
            }
            if !points.iter().any(|p| p.up <= 95 && p.duty >= 160) {
                return Err(format!(
                    "{name}: reach at least 80% duty by 95°C, before any 255 sentinel entries."
                ));
            }
        }
        Ok(())
    }

    pub fn validate_against(&self, current: &Self) -> Result<()> {
        self.validate()?;
        for (points, reference) in [(&self.cpu, &current.cpu), (&self.gpu, &current.gpu)] {
            if reference.len() != 16 {
                return Err("No complete firmware reference curve is available.".into());
            }
            for (p, old) in points.iter().zip(reference) {
                // The inspected factory GPU table contains a 70/71 threshold pair; preserve it.
                if p.down > p.up && (p.up != old.up || p.down != old.down) {
                    return Err("New falling thresholds must not exceed rising thresholds. Existing firmware pairs may be retained unchanged.".into());
                }
            }
        }
        Ok(())
    }

    pub fn writes(&self) -> Result<Vec<(u16, u8)>> {
        self.validate()?;
        let mut writes = Vec::with_capacity(96);
        for (fan, points) in [&self.cpu, &self.gpu].iter().enumerate() {
            for (i, p) in points.iter().enumerate() {
                for (column, value) in [p.up, p.down, p.duty].into_iter().enumerate() {
                    writes.push((FAN_BASES[fan * 3 + column] + i as u16, value));
                }
            }
        }
        Ok(writes)
    }
}

pub fn parse_duty(value: &str) -> Result<u8> {
    let percent: f64 = value
        .trim()
        .parse()
        .map_err(|_| "Enter a fan percentage.".to_string())?;
    let raw = percent * 2.0;
    if !raw.is_finite() || !(0.0..=200.0).contains(&raw) || raw.fract() != 0.0 {
        return Err("Fan duty must be 0–100%, in steps of 0.5%.".into());
    }
    Ok(raw as u8)
}

pub fn validate_cpu(pl1: u8, pl2: u8, ceiling: (u8, u8)) -> Result<()> {
    if pl1 == 0 || pl2 == 0 || pl1 > pl2 || pl1 > ceiling.0 || pl2 > ceiling.1 {
        return Err(format!(
            "Use 1 <= PL1 <= PL2, within the firmware performance preset ({}/{} W).",
            ceiling.0, ceiling.1
        ));
    }
    Ok(())
}

pub fn cpu_writes(current_pl2: u8, pl1: u8, pl2: u8) -> Vec<(u16, u8)> {
    if pl1 > current_pl2 {
        vec![(0x0784, pl2), (0x0783, pl1)]
    } else {
        vec![(0x0783, pl1), (0x0784, pl2)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fan_boost_turns_on_then_off_without_changing_other_bits() {
        for original in [0x00, 0x10, 0x80, 0x90] {
            let on = toggle_boost(original);
            assert_eq!(on & 0x40, 0x40);
            assert_eq!(on & !0x40, original);
            assert_eq!(toggle_boost(on), original);
        }
    }

    pub fn curve() -> FanCurve {
        let points: Vec<_> = (0..16)
            .map(|i| FanPoint {
                up: 20 + i * 5,
                down: 17 + i * 5,
                duty: 50 + i * 10,
            })
            .collect();
        FanCurve {
            cpu: points.clone(),
            gpu: points,
        }
    }

    #[test]
    fn curve_preserves_units_and_addresses() {
        let writes = curve().writes().unwrap();
        assert_eq!(writes.len(), 96);
        assert!(writes.contains(&(0x0F08, 60)));
        assert!(writes.contains(&(0x0F28, 130)));
        assert!(writes.contains(&(0x0F5F, 200)));
    }

    #[test]
    fn reject_invalid_curves_before_hardware_access() {
        let mut c = curve();
        c.cpu.pop();
        assert!(c.writes().is_err());
        let mut c = curve();
        c.gpu[5].down = 100;
        assert!(c.validate_against(&curve()).is_err());
        let mut c = curve();
        c.cpu[8].duty = 0;
        assert!(c.writes().is_err());
    }
    #[test]
    fn preserve_observed_factory_threshold_overlap_without_allowing_new_ones() {
        let mut factory = curve();
        factory.gpu[10].down = factory.gpu[10].up + 1;
        assert!(factory.validate_against(&factory).is_ok());
        let mut edited = factory.clone();
        edited.gpu[10].down += 1;
        assert!(edited.validate_against(&factory).is_err());
    }
    #[test]
    fn an_unreachable_sentinel_is_not_a_high_temperature_cooling_guarantee() {
        let mut c = curve();
        for p in &mut c.cpu {
            p.duty = 0;
        }
        c.cpu[15] = FanPoint {
            up: 255,
            down: 255,
            duty: 200,
        };
        assert!(c.validate().is_err());
    }
    #[test]
    fn high_temperature_entries_are_allowed_after_the_cooling_floor() {
        let mut c = curve();
        c.cpu[15].up = 100;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn percentage_parser_preserves_half_percent_and_rejects_nan() {
        assert_eq!(parse_duty("50.5").unwrap(), 101);
        for value in ["NaN", "inf", "-1", "100.5", "50.25", "text"] {
            assert!(parse_duty(value).is_err(), "{value}");
        }
    }

    #[test]
    fn power_limits_respect_firmware_and_each_other() {
        assert!(validate_cpu(45, 90, (65, 100)).is_ok());
        for values in [(0, 90), (90, 45), (66, 100), (65, 101)] {
            assert!(validate_cpu(values.0, values.1, (65, 100)).is_err());
        }
    }
    #[test]
    fn raising_both_limits_writes_the_burst_ceiling_first() {
        assert_eq!(cpu_writes(40, 65, 90), vec![(0x0784, 90), (0x0783, 65)]);
        assert_eq!(cpu_writes(90, 35, 60), vec![(0x0783, 35), (0x0784, 60)]);
    }
}
