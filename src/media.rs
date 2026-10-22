//! Media access: ffprobe stream info and HEVC bitstream sampling (via ffmpeg or raw files).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::nal::NalSplitter;
use crate::rpu::RpuStats;

const READ_CHUNK: usize = 1 << 20;
/// Upper bound of raw bitstream bytes read per sample point
const RAW_SAMPLE_MAX_BYTES: u64 = 256 << 20;

#[derive(Debug, Clone)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// Dolby Vision decoder configuration record (dvcC / dvvC / registration descriptor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoviConfig {
    pub profile: u8,
    pub level: u8,
    pub rpu_present: bool,
    pub el_present: bool,
    pub bl_present: bool,
    pub bl_compat_id: u8,
}

#[derive(Debug, Clone)]
pub struct VideoStream {
    /// Absolute stream index in the container
    pub index: u64,
    pub codec: String,
    pub frame_rate: Option<f64>,
    pub dovi: Option<DoviConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct ProbeInfo {
    pub duration: Option<f64>,
    pub video_streams: Vec<VideoStream>,
}

impl ProbeInfo {
    pub fn dovi(&self) -> Option<DoviConfig> {
        self.video_streams.iter().find_map(|s| s.dovi)
    }

    pub fn hevc_streams(&self) -> impl Iterator<Item = &VideoStream> {
        self.video_streams.iter().filter(|s| s.codec == "hevc")
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn as_flag(v: &Value) -> bool {
    as_u64(v).unwrap_or(0) != 0
}

/// Parses ffprobe rationals such as `24000/1001`
fn parse_rate(v: &Value) -> Option<f64> {
    let (num, den) = v.as_str()?.split_once('/')?;
    let rate = num.parse::<f64>().ok()? / den.parse::<f64>().ok()?;
    (rate.is_finite() && rate > 0.0).then_some(rate)
}

fn parse_dovi_side_data(side_data: &Value) -> Option<DoviConfig> {
    side_data.as_array()?.iter().find_map(|sd| {
        if sd["side_data_type"].as_str()? != "DOVI configuration record" {
            return None;
        }

        Some(DoviConfig {
            profile: as_u64(&sd["dv_profile"])? as u8,
            level: as_u64(&sd["dv_level"]).unwrap_or(0) as u8,
            rpu_present: as_flag(&sd["rpu_present_flag"]),
            el_present: as_flag(&sd["el_present_flag"]),
            bl_present: as_flag(&sd["bl_present_flag"]),
            bl_compat_id: as_u64(&sd["dv_bl_signal_compatibility_id"]).unwrap_or(0) as u8,
        })
    })
}

/// Parses `ffprobe -show_streams -show_format -of json` output.
pub fn parse_ffprobe_json(json: &str) -> Result<ProbeInfo> {
    let root: Value = serde_json::from_str(json).context("Invalid ffprobe JSON output")?;

    let duration = root["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0);

    let video_streams = root["streams"]
        .as_array()
        .map(|streams| {
            streams
                .iter()
                .filter(|s| s["codec_type"] == "video")
                .filter(|s| !as_flag(&s["disposition"]["attached_pic"]))
                .filter_map(|s| {
                    Some(VideoStream {
                        index: as_u64(&s["index"])?,
                        codec: s["codec_name"].as_str().unwrap_or("unknown").to_owned(),
                        frame_rate: parse_rate(&s["avg_frame_rate"])
                            .or_else(|| parse_rate(&s["r_frame_rate"])),
                        dovi: parse_dovi_side_data(&s["side_data_list"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ProbeInfo {
        duration,
        video_streams,
    })
}

pub fn ffprobe(tools: &Tools, path: &Path) -> Result<ProbeInfo> {
    let output = Command::new(&tools.ffprobe)
        .args([
            "-v",
            "error",
            "-probesize",
            "50M",
            "-analyzeduration",
            "20M",
        ])
        .args(["-show_streams", "-show_format", "-of", "json"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("Failed to run {}", tools.ffprobe.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffprobe failed: {}", stderr.trim());
    }

    parse_ffprobe_json(&String::from_utf8_lossy(&output.stdout))
}

/// Streams `reader` through the NAL splitter into `stats`.
/// Stops after `max_rpus` new RPUs or `max_bytes` bytes.
/// Returns `true` if stopped early.
fn feed_stats(
    reader: &mut impl Read,
    stats: &mut RpuStats,
    max_rpus: u64,
    max_bytes: u64,
) -> Result<bool> {
    let rpus_before = stats.rpus + stats.parse_errors;
    let mut splitter = NalSplitter::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut total = 0u64;

    let mut on_nal = |nal: &[u8]| {
        stats.handle_nal(nal);
        stats.rpus + stats.parse_errors - rpus_before < max_rpus
    };

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };

        if !splitter.push(&buf[..n], &mut on_nal) {
            return Ok(true);
        }

        total += n as u64;
        if total >= max_bytes {
            return Ok(true);
        }
    }

    splitter.finish(&mut on_nal);
    Ok(false)
}

/// Sampling options shared by all extraction methods.
#[derive(Debug, Clone, Copy)]
pub struct SampleOptions {
    /// Number of positions sampled across the file
    pub samples: u32,
    /// RPUs (frames) read per position
    pub frames: u32,
    /// Skip the remaining sample positions once FEL has been found
    pub fast: bool,
}

/// Evenly spread sample positions as fractions of the file, e.g. 3 => 0, 0.33, 0.67.
/// The first sample always starts at the beginning.
fn sample_fractions(samples: u32) -> Vec<f64> {
    let samples = samples.max(1);
    (0..samples).map(|i| i as f64 / samples as f64).collect()
}

/// Samples a container stream by remuxing it to an Annex B HEVC bitstream with ffmpeg.
pub fn sample_with_ffmpeg(
    tools: &Tools,
    path: &Path,
    stream_index: u64,
    duration: Option<f64>,
    opts: SampleOptions,
    stats: &mut RpuStats,
) -> Result<()> {
    let fractions = match duration {
        Some(_) => sample_fractions(opts.samples),
        None => vec![0.0],
    };

    for fraction in fractions {
        let mut cmd = Command::new(&tools.ffmpeg);
        cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);

        if let Some(duration) = duration.filter(|_| fraction > 0.0) {
            cmd.arg("-ss").arg(format!("{:.3}", duration * fraction));
        }

        cmd.arg("-i")
            .arg(path)
            .arg("-map")
            .arg(format!("0:{stream_index}"))
            .args(["-c:v", "copy", "-bsf:v", "hevc_mp4toannexb"])
            .arg("-frames:v")
            .arg(opts.frames.to_string())
            .args(["-f", "hevc", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to run {}", tools.ffmpeg.display()))?;

        let mut stdout = child.stdout.take().expect("piped stdout");
        let result = feed_stats(&mut stdout, stats, opts.frames as u64, u64::MAX);
        drop(stdout);

        let stopped_early = matches!(result, Ok(true));
        if stopped_early {
            let _ = child.kill();
        }

        let output = child.wait_with_output()?;
        result?;

        if !stopped_early && !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("ffmpeg failed: {}", stderr.trim());
        }

        if opts.fast && stats.has_fel() {
            break;
        }
    }

    Ok(())
}

/// Samples a raw Annex B HEVC bitstream file directly, without ffmpeg.
pub fn sample_raw_hevc(path: &Path, opts: SampleOptions, stats: &mut RpuStats) -> Result<()> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();

    for fraction in sample_fractions(opts.samples) {
        file.seek(SeekFrom::Start((len as f64 * fraction) as u64))?;
        feed_stats(&mut file, stats, opts.frames as u64, RAW_SAMPLE_MAX_BYTES)?;

        if opts.fast && stats.has_fel() {
            break;
        }
    }

    Ok(())
}

pub fn tool_available(tool: &Path) -> bool {
    Command::new(tool)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_dovi_record() {
        let json = r#"{
            "streams": [
                {
                    "index": 0, "codec_name": "hevc", "codec_type": "video",
                    "width": 3840, "height": 2160, "avg_frame_rate": "24000/1001",
                    "disposition": { "attached_pic": 0 },
                    "side_data_list": [{
                        "side_data_type": "DOVI configuration record",
                        "dv_version_major": 1, "dv_version_minor": 0,
                        "dv_profile": 7, "dv_level": 6,
                        "rpu_present_flag": 1, "el_present_flag": 1, "bl_present_flag": 1,
                        "dv_bl_signal_compatibility_id": 6
                    }]
                },
                { "index": 1, "codec_name": "truehd", "codec_type": "audio" },
                {
                    "index": 2, "codec_name": "mjpeg", "codec_type": "video",
                    "disposition": { "attached_pic": 1 }
                }
            ],
            "format": { "duration": "7200.500000" }
        }"#;

        let info = parse_ffprobe_json(json).unwrap();
        assert_eq!(info.duration, Some(7200.5));
        assert_eq!(info.video_streams.len(), 1);
        assert_eq!(info.hevc_streams().count(), 1);
        assert!((info.video_streams[0].frame_rate.unwrap() - 23.976).abs() < 0.001);
        assert_eq!(
            info.dovi(),
            Some(DoviConfig {
                profile: 7,
                level: 6,
                rpu_present: true,
                el_present: true,
                bl_present: true,
                bl_compat_id: 6,
            })
        );
    }

    #[test]
    fn parses_ffprobe_without_dovi() {
        let json =
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video"}],"format":{}}"#;
        let info = parse_ffprobe_json(json).unwrap();
        assert_eq!(info.duration, None);
        assert_eq!(info.dovi(), None);
        assert_eq!(info.hevc_streams().count(), 0);
        assert_eq!(info.video_streams[0].frame_rate, None);
    }

    #[test]
    fn fractions() {
        assert_eq!(sample_fractions(0), vec![0.0]);
        assert_eq!(sample_fractions(4), vec![0.0, 0.25, 0.5, 0.75]);
    }
}
