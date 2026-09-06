//! Peak meter scale and hold logic shared by the editor.

/// Lowest displayed level. Anything quieter sits on the floor.
pub const METER_FLOOR_DB: f32 = -60.0;

/// Linear peak → dBFS, clamped to `[METER_FLOOR_DB, 6]`.
pub fn peak_to_db(peak: f32) -> f32 {
    if !peak.is_finite() || peak <= 1.0e-6 {
        return METER_FLOOR_DB;
    }
    (20.0 * peak.log10()).clamp(METER_FLOOR_DB, 6.0)
}

/// dBFS → `[0, 1]` rail position. Zero dBFS is full scale.
pub fn db_to_pos(db: f32) -> f32 {
    ((db.clamp(METER_FLOOR_DB, 0.0) - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

/// Peak-hold that parks for ~0.9 s, then decays.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PeakHold {
    peak: f32,
    age: f32,
}

impl PeakHold {
    const PARK: f32 = 0.9;
    const STEP: f32 = 0.033;
    const DECAY: f32 = 0.82;

    /// Feed one frame's peak; returns the held peak to draw.
    pub fn update(&mut self, peak: f32) -> f32 {
        if peak >= self.peak {
            self.peak = peak;
            self.age = 0.0;
        } else {
            self.age += Self::STEP;
            if self.age > Self::PARK {
                self.peak *= Self::DECAY;
            }
        }
        self.peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_is_log_dbfs() {
        assert!((peak_to_db(1.0)).abs() < 0.01);
        assert!((peak_to_db(0.5) + 6.02).abs() < 0.05);
        assert_eq!(peak_to_db(0.0), METER_FLOOR_DB);
        assert_eq!(peak_to_db(f32::NAN), METER_FLOOR_DB);
        assert!(db_to_pos(-6.0) > db_to_pos(-18.0));
        assert!(db_to_pos(-18.0) > 0.5);
        assert!((db_to_pos(0.0) - 1.0).abs() < f32::EPSILON);
        assert!(db_to_pos(-60.0).abs() < f32::EPSILON);
    }

    #[test]
    fn hold_parks_then_decays() {
        let mut hold = PeakHold::default();
        assert_eq!(hold.update(0.8), 0.8);
        for _ in 0..20 {
            assert_eq!(hold.update(0.1), 0.8, "held during park window");
        }
        for _ in 0..20 {
            hold.update(0.1);
        }
        assert!(hold.update(0.1) < 0.8);
        assert_eq!(hold.update(0.9), 0.9, "a louder peak re-arms immediately");
    }
}
