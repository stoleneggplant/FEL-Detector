//! Per-file classification: FEL, MEL, other Dolby Vision, or no Dolby Vision.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::media::{self, DoviConfig, ProbeInfo, SampleOptions, Tools};
use crate::rpu::RpuStats;

/// Raw Annex B HEVC bitstreams, read directly without ffmpeg
const RAW_HEVC_EXTENSIONS: &[&str] = &["hevc", "h265", "265"];
/// Containers in which ffprobe may not expose the Dolby Vision configuration
/// (e.g. Blu-ray dual PID streams), so the RPUs are always inspected
const TRANSPORT_STREAM_EXTENSIONS: &[&str] = &["ts", "m2ts", "mts"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    /// Dolby Vision with a full enhancement layer
    Fel,
    /// Dolby Vision with a minimal enhancement layer
    Mel,
    /// Dolby Vision without an enhancement layer (profiles 5, 8, 10...)
    DolbyVision,
    NoDolbyVision,
    /// Dolby Vision signaled, but the enhancement layer type could not be determined
    Unknown,
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Fel => "FEL",
            Status::Mel => "MEL",
            Status::DolbyVision => "DV",
            Status::NoDolbyVision => "NO DV",
            Status::Unknown => "UNKNOWN",
            Status::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub path: PathBuf,
    pub status: Status,
    /// Dolby Vision profile, from the configuration record or the RPUs
    pub profile: Option<u8>,
    /// Base layer signal compatibility ID (e.g. 1 for profile 8.1)
    pub bl_compat_id: Option<u8>,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpu_stats: Option<RpuStats>,
}

impl Report {
    pub fn is_fel(&self) -> bool {
        self.status == Status::Fel
    }

    /// Human readable profile, e.g. `P7` or `P8.1`
    pub fn profile_label(&self) -> String {
        match (self.profile, self.bl_compat_id) {
            (None, _) => String::new(),
            (Some(p @ (8 | 10)), Some(id)) => format!("P{p}.{id}"),
            (Some(p), _) => format!("P{p}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DetectOptions {
    pub tools: Tools,
    pub sample: SampleOptions,
    /// Inspect RPUs of every HEVC stream, even when the container
    /// signals no Dolby Vision or no enhancement layer
    pub deep: bool,
}

fn has_extension(path: &Path, list: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| list.iter().any(|l| l.eq_ignore_ascii_case(e)))
}

pub fn detect(path: &Path, opts: &DetectOptions) -> Report {
    match try_detect(path, opts) {
        Ok(report) => report,
        Err(e) => Report {
            path: path.to_owned(),
            status: Status::Error,
            profile: None,
            bl_compat_id: None,
            detail: format!("{e:#}"),
            rpu_stats: None,
        },
    }
}

/// Raw HEVC bitstreams are analyzed without ffmpeg.
pub fn is_raw_hevc(path: &Path) -> bool {
    has_extension(path, RAW_HEVC_EXTENSIONS)
}

fn try_detect(path: &Path, opts: &DetectOptions) -> anyhow::Result<Report> {
    if is_raw_hevc(path) {
        let mut stats = RpuStats::default();
        media::sample_raw_hevc(path, opts.sample, &mut stats)?;
        return Ok(verdict(path, stats, None));
    }

    let info = media::ffprobe(&opts.tools, path)?;
    detect_container(path, &info, opts)
}

fn detect_container(path: &Path, info: &ProbeInfo, opts: &DetectOptions) -> anyhow::Result<Report> {
    let dovi = info.dovi();
    let report = |status, detail: String| Report {
        path: path.to_owned(),
        status,
        profile: dovi.map(|c| c.profile),
        bl_compat_id: dovi.map(|c| c.bl_compat_id),
        detail,
        rpu_stats: None,
    };

    if info.hevc_streams().next().is_none() {
        return Ok(match dovi {
            Some(_) => report(
                Status::DolbyVision,
                "non-HEVC Dolby Vision, no enhancement layer".into(),
            ),
            None if info.video_streams.is_empty() => {
                report(Status::NoDolbyVision, "no video stream".into())
            }
            None => report(Status::NoDolbyVision, "no HEVC video stream".into()),
        });
    }

    let needs_rpu_scan = match dovi {
        Some(c) => opts.deep || c.el_present || matches!(c.profile, 4 | 7),
        None => opts.deep || has_extension(path, TRANSPORT_STREAM_EXTENSIONS),
    };

    if !needs_rpu_scan {
        return Ok(match dovi {
            Some(_) => report(Status::DolbyVision, "no enhancement layer signaled".into()),
            None => report(
                Status::NoDolbyVision,
                "no Dolby Vision configuration".into(),
            ),
        });
    }

    // Streams signaling Dolby Vision first. For Blu-ray dual PID streams the RPUs
    // are carried in the (second) enhancement layer stream.
    let mut candidates: Vec<_> = info.hevc_streams().collect();
    candidates.sort_by_key(|s| s.dovi.is_none());

    let mut stats = RpuStats::default();
    for stream in candidates {
        media::sample_with_ffmpeg(
            &opts.tools,
            path,
            stream.index,
            info.duration,
            opts.sample,
            &mut stats,
        )?;

        if stats.rpus > 0 {
            break;
        }
    }

    Ok(verdict(path, stats, dovi))
}

fn verdict(path: &Path, stats: RpuStats, dovi: Option<DoviConfig>) -> Report {
    let sampled = stats.rpus;
    let (status, detail) = if stats.fel_rpus > 0 {
        (
            Status::Fel,
            format!(
                "full enhancement layer, {}/{sampled} sampled RPUs are FEL",
                stats.fel_rpus
            ),
        )
    } else if stats.mel_rpus > 0 {
        (
            Status::Mel,
            format!("minimal enhancement layer, {sampled} RPUs sampled"),
        )
    } else if sampled > 0 {
        (
            Status::DolbyVision,
            format!("no enhancement layer, {sampled} RPUs sampled"),
        )
    } else if let Some(c) = dovi {
        let msg = if stats.parse_errors > 0 {
            format!("{} RPUs found but none could be parsed", stats.parse_errors)
        } else {
            format!(
                "profile {} signaled but no RPU found in sampled frames",
                c.profile
            )
        };
        (Status::Unknown, msg)
    } else {
        (Status::NoDolbyVision, "no Dolby Vision RPU found".into())
    };

    let profile = stats.dominant_profile().or(dovi.map(|c| c.profile));
    let bl_compat_id = dovi.map(|c| c.bl_compat_id);

    Report {
        path: path.to_owned(),
        status,
        profile,
        bl_compat_id,
        detail,
        rpu_stats: Some(stats),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpu::tests::profile7_rpu_nalu;

    fn stats_with(fel: bool, count: usize) -> RpuStats {
        let mut stats = RpuStats::default();
        for _ in 0..count {
            stats.handle_nal(&profile7_rpu_nalu(fel));
        }
        stats
    }

    #[test]
    fn verdicts() {
        let p = Path::new("x.mkv");
        assert_eq!(verdict(p, stats_with(true, 2), None).status, Status::Fel);

        let mel = verdict(p, stats_with(false, 2), None);
        assert_eq!(mel.status, Status::Mel);
        assert_eq!(mel.profile_label(), "P7");

        assert_eq!(
            verdict(p, RpuStats::default(), None).status,
            Status::NoDolbyVision
        );

        let dovi = DoviConfig {
            profile: 7,
            level: 6,
            rpu_present: true,
            el_present: true,
            bl_present: true,
            bl_compat_id: 6,
        };
        assert_eq!(
            verdict(p, RpuStats::default(), Some(dovi)).status,
            Status::Unknown
        );
    }

    #[test]
    fn mixed_samples_are_fel() {
        let mut stats = stats_with(false, 5);
        stats.handle_nal(&profile7_rpu_nalu(true));
        let report = verdict(Path::new("x.mkv"), stats, None);
        assert!(report.is_fel());
        assert!(report.detail.contains("1/6"));
    }

    #[test]
    fn extensions() {
        assert!(has_extension(
            Path::new("a/B.M2TS"),
            TRANSPORT_STREAM_EXTENSIONS
        ));
        assert!(!has_extension(
            Path::new("a/b.mkv"),
            TRANSPORT_STREAM_EXTENSIONS
        ));
        assert!(!has_extension(Path::new("hevc"), RAW_HEVC_EXTENSIONS));
    }
}
