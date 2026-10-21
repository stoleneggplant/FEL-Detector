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
}

impl RpuStats {
    /// Processes one HEVC NAL unit (including its 2 byte header).
    pub fn handle_nal(&mut self, nal: &[u8]) {
        match hevc_nal_type(nal) {
            Some(NAL_UNSPEC62_RPU) => self.handle_rpu(nal),
            Some(NAL_UNSPEC63_EL) => self.el_nalus += 1,
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
    fn counts_errors_and_el() {
        let mut stats = RpuStats::default();
        stats.handle_nal(&[0x7C, 0x01, 0x00, 0x00]);
        stats.handle_nal(&[0x7E, 0x01, 0x40, 0x01]);
        assert_eq!(stats.parse_errors, 1);
        assert_eq!(stats.el_nalus, 1);
    }
}
