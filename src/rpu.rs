//! Dolby Vision RPU analysis: counts FEL / MEL frames in a HEVC bitstream.

use std::collections::BTreeMap;

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::rpu_data_nlq::DoviELType;
use serde::Serialize;

use crate::nal::{NAL_UNSPEC62_RPU, NAL_UNSPEC63_EL, hevc_nal_type};

/// Statistics gathered from the RPUs (and EL NAL units) of a sampled bitstream.
#[derive(Debug, Default, Clone, Serialize)]
pub struct RpuStats {
    /// Number of RPU NAL units successfully parsed
    pub rpus: u64,
    /// Number of RPU NAL units that failed to parse
    pub parse_errors: u64,
    /// RPUs signaling a full enhancement layer (non-trivial NLQ residual)
    pub fel_rpus: u64,
    /// RPUs signaling a minimal enhancement layer (NLQ residual is a no-op)
    pub mel_rpus: u64,
    /// UNSPEC63 NAL units (enhancement layer wrapped in a single track)
    pub el_nalus: u64,
    /// Dolby Vision profiles as derived from the RPU headers
    pub profiles: BTreeMap<u8, u64>,
    /// Pictures (first slice segments of unwrapped VCL NAL units)
    pub frames: u64,
    /// Bytes of all NAL units
    pub total_bytes: u64,
    /// Bytes of UNSPEC63 (wrapped enhancement layer) NAL units
    pub el_bytes: u64,
    /// Bytes of RPU NAL units
    pub rpu_bytes: u64,
}

impl RpuStats {
    /// Processes one HEVC NAL unit (including its 2 byte header).
    pub fn handle_nal(&mut self, nal: &[u8]) {
        let len = nal.len() as u64;
        self.total_bytes += len;

        match hevc_nal_type(nal) {
            Some(NAL_UNSPEC62_RPU) => {
                self.rpu_bytes += len;
                self.handle_rpu(nal);
            }
            Some(NAL_UNSPEC63_EL) => {
                self.el_nalus += 1;
                self.el_bytes += len;
            }
            // VCL NAL unit with first_slice_segment_in_pic_flag set
            Some(0..32) if nal.get(2).is_some_and(|b| b & 0x80 != 0) => self.frames += 1,
            _ => {}
        }
    }

    fn handle_rpu(&mut self, nal: &[u8]) {
        let rpu = match DoviRpu::parse_unspec62_nalu(nal) {
            Ok(rpu) => rpu,
            Err(_) => {
                self.parse_errors += 1;
                return;
            }
        };

        self.rpus += 1;
        *self.profiles.entry(rpu.dovi_profile).or_default() += 1;

        match rpu.el_type {
            Some(DoviELType::FEL) => self.fel_rpus += 1,
            Some(DoviELType::MEL) => self.mel_rpus += 1,
            None => {}
        }
    }

    /// Most common profile seen in the RPUs.
    pub fn dominant_profile(&self) -> Option<u8> {
        self.profiles
            .iter()
            .max_by_key(|(_, count)| **count)
            .map(|(profile, _)| *profile)
    }

    pub fn has_fel(&self) -> bool {
        self.fel_rpus > 0
    }

    /// RPUs carrying an enhancement layer description (profiles 4 and 7)
    pub fn has_dual_layer_rpus(&self) -> bool {
        self.fel_rpus + self.mel_rpus > 0
    }

    /// Estimated enhancement layer bitrate in bits per second.
    ///
    /// For single track dual layer streams the EL is the UNSPEC63 NAL units.
    /// With `el_only_stream` (separate EL stream, e.g. Blu-ray dual PID) every
    /// NAL unit except the RPUs belongs to the EL.
    pub fn el_bitrate(&self, fps: f64, el_only_stream: bool) -> Option<f64> {
        if self.frames == 0 || !(fps.is_finite() && fps > 0.0) {
            return None;
        }

        let bytes = if self.el_nalus > 0 {
            self.el_bytes
        } else if el_only_stream {
            self.total_bytes - self.rpu_bytes
        } else {
            return None;
        };

        Some(bytes as f64 * 8.0 * fps / self.frames as f64)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dolby_vision::rpu::generate::GenerateConfig;

    /// Builds a profile 7 RPU NAL unit (with 0x7C01 header), either FEL or MEL.
    pub fn profile7_rpu_nalu(fel: bool) -> Vec<u8> {
        let config = GenerateConfig {
            length: 1,
            ..Default::default()
        };
        let mut rpu = DoviRpu::profile81_config(&config).unwrap();
        rpu.convert_with_mode(1).unwrap(); // to MEL, adds NLQ data

        rpu.dovi_profile = 7;
        rpu.header.vdr_bit_depth_minus8 = 4; // 12 bit VDR => profile 7

        if fel {
            let nlq = rpu.rpu_data_mapping.as_mut().unwrap().nlq.as_mut().unwrap();
            nlq.nlq_offset = [512, 512, 512];
            nlq.vdr_in_max_int = [0, 0, 0];
            nlq.vdr_in_max = [5_000, 6_000, 7_000];
            nlq.linear_deadzone_slope = [2_048, 2_048, 2_048];
            nlq.linear_deadzone_threshold = [100, 100, 100];
        }

        rpu.write_hevc_unspec62_nalu().unwrap()
    }

    pub fn profile81_rpu_nalu() -> Vec<u8> {
        let config = GenerateConfig {
            length: 1,
            ..Default::default()
        };
        DoviRpu::profile81_config(&config)
            .unwrap()
            .write_hevc_unspec62_nalu()
            .unwrap()
    }

    #[test]
    fn detects_fel() {
        let mut stats = RpuStats::default();
        stats.handle_nal(&profile7_rpu_nalu(true));
        assert_eq!(stats.rpus, 1);
        assert_eq!(stats.fel_rpus, 1);
        assert_eq!(stats.mel_rpus, 0);
        assert_eq!(stats.dominant_profile(), Some(7));
        assert!(stats.has_fel());
    }

    #[test]
    fn detects_mel() {
        let mut stats = RpuStats::default();
        stats.handle_nal(&profile7_rpu_nalu(false));
        assert_eq!(stats.rpus, 1);
        assert_eq!(stats.fel_rpus, 0);
        assert_eq!(stats.mel_rpus, 1);
        assert_eq!(stats.dominant_profile(), Some(7));
    }

    #[test]
    fn profile8_has_no_el() {
        let mut stats = RpuStats::default();
        stats.handle_nal(&profile81_rpu_nalu());
        assert_eq!(stats.rpus, 1);
        assert_eq!(stats.fel_rpus + stats.mel_rpus, 0);
        assert_eq!(stats.dominant_profile(), Some(8));
    }

    #[test]
    fn el_bitrate() {
        let mut stats = RpuStats::default();
        for _ in 0..24 {
            stats.handle_nal(&[0x26, 0x01, 0x80, 0x00]); // IDR, first slice
            stats.handle_nal(&[0x02, 0x01, 0x00, 0x00]); // trailing slice, not first
            stats.handle_nal(&[0x7E, 0x01, 0x00, 0x00, 0x00, 0x00]); // 6 bytes EL
        }

        assert_eq!(stats.frames, 24);
        assert_eq!(stats.el_bytes, 24 * 6);
        // 6 bytes per frame at 24 fps
        assert_eq!(stats.el_bitrate(24.0, false), Some(6.0 * 8.0 * 24.0));
        assert_eq!(stats.el_bitrate(0.0, false), None);

        let mut el_stream = RpuStats::default();
        el_stream.handle_nal(&[0x26, 0x01, 0x80, 0x00]);
        assert_eq!(el_stream.el_bitrate(1.0, false), None);
        assert_eq!(el_stream.el_bitrate(1.0, true), Some(32.0));
    }

    #[test]
    fn counts_errors_and_el() {
        let mut stats = RpuStats::default();
        stats.handle_nal(&[0x7C, 0x01, 0x00, 0x00]);
        stats.handle_nal(&[0x7E, 0x01, 0x40, 0x01]);
        assert_eq!(stats.parse_errors, 1);
        assert_eq!(stats.el_nalus, 1);
    }
}
