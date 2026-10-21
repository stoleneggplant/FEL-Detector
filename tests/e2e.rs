//! End to end tests: encodes small HEVC clips with ffmpeg/libx265, injects
//! Dolby Vision RPUs and runs the detector binary on the resulting files.
//! Skipped when ffmpeg with libx265 is not available.

use std::path::{Path, PathBuf};
use std::process::Command;

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::generate::GenerateConfig;
use fel_detector::nal::{NalSplitter, hevc_nal_type};
use serde_json::Value;

const FRAMES: usize = 96;

#[derive(Clone, Copy)]
enum Rpu {
    Fel,
    Mel,
    P81,
}

fn rpu_nalu(kind: Rpu) -> Vec<u8> {
    let config = GenerateConfig {
        length: 1,
        ..Default::default()
    };
    let mut rpu = DoviRpu::profile81_config(&config).unwrap();

    if let Rpu::Fel | Rpu::Mel = kind {
        rpu.convert_with_mode(1).unwrap();
        rpu.dovi_profile = 7;
        rpu.header.vdr_bit_depth_minus8 = 4;
    }
    if let Rpu::Fel = kind {
        let nlq = rpu.rpu_data_mapping.as_mut().unwrap().nlq.as_mut().unwrap();
        nlq.nlq_offset = [512, 512, 512];
        nlq.vdr_in_max_int = [0, 0, 0];
        nlq.vdr_in_max = [5_000, 6_000, 7_000];
        nlq.linear_deadzone_slope = [2_048, 2_048, 2_048];
    }

    rpu.write_hevc_unspec62_nalu().unwrap()
}

fn ffmpeg_with_x265() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("libx265"))
}

fn ffmpeg(args: &[&str]) {
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg {args:?} failed");
}

fn encode_base(dir: &Path) -> Vec<u8> {
    let out = dir.join("base.hevc");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=24",
        "-frames:v",
        &FRAMES.to_string(),
        "-pix_fmt",
        "yuv420p10le",
        "-c:v",
        "libx265",
        "-x265-params",
        "log-level=none:keyint=24:min-keyint=24:bframes=0",
        "-f",
        "hevc",
        out.to_str().unwrap(),
    ]);
    std::fs::read(out).unwrap()
}

/// Appends an RPU after every VCL NAL unit (single slice per frame).
/// `rpu_for_frame` chooses the RPU type per frame, `None` skips it.
fn inject_rpus(base: &[u8], rpu_for_frame: impl Fn(usize) -> Option<Rpu>) -> Vec<u8> {
    let mut nals = Vec::new();
    let mut splitter = NalSplitter::new();
    splitter.push(base, |n| {
        nals.push(n.to_vec());
        true
    });
    splitter.finish(|n| {
        nals.push(n.to_vec());
        true
    });

    let mut out = Vec::new();
    let mut frame = 0;
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&nal);

        if hevc_nal_type(&nal).unwrap() < 32 {
            if let Some(kind) = rpu_for_frame(frame) {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(&rpu_nalu(kind));
            }
            frame += 1;
        }
    }
    assert_eq!(frame, FRAMES);
    out
}

fn write_variants(dir: &Path, name: &str, stream: &[u8], containers: &[&str]) {
    let raw = dir.join(format!("{name}.hevc"));
    std::fs::write(&raw, stream).unwrap();

    for ext in containers {
        let out = dir.join(format!("{name}.{ext}"));
        ffmpeg(&[
            "-f",
            "hevc",
            "-r",
            "24",
            "-i",
            raw.to_str().unwrap(),
            "-c",
            "copy",
            out.to_str().unwrap(),
        ]);
    }
}

fn run_detector(args: &[&str]) -> Vec<Value> {
    let output = Command::new(env!("CARGO_BIN_EXE_fel-detector"))
        .args(["--format", "json", "--quiet"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "detector failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

fn status_of<'a>(reports: &'a [Value], file: &str) -> &'a str {
    reports
        .iter()
        .find(|r| r["path"].as_str().unwrap().ends_with(file))
        .unwrap_or_else(|| panic!("{file} not in report"))["status"]
        .as_str()
        .unwrap()
}

fn setup() -> Option<PathBuf> {
    if !ffmpeg_with_x265() {
        eprintln!("skipping: ffmpeg with libx265 not available");
        return None;
    }

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("media")).unwrap();
    let media = dir.join("media");

    let base = encode_base(&dir);
    let both = &["mkv", "ts"];
    write_variants(&media, "fel", &inject_rpus(&base, |_| Some(Rpu::Fel)), both);
    write_variants(&media, "mel", &inject_rpus(&base, |_| Some(Rpu::Mel)), both);
    write_variants(&media, "p81", &inject_rpus(&base, |_| Some(Rpu::P81)), both);
    write_variants(&media, "sdr", &base, both);
    // FEL only in the last third of the file, found by the later sample positions
    let late = inject_rpus(&base, |f| {
        Some(if f >= FRAMES * 2 / 3 {
            Rpu::Fel
        } else {
            Rpu::Mel
        })
    });
    write_variants(&media, "late_fel", &late, &["ts"]);

    std::fs::write(media.join("notes.txt"), "not a video").unwrap();
    Some(dir)
}

#[test]
fn detects_fel_files_in_directory() {
    let Some(dir) = setup() else { return };
    let media = dir.join("media");
    let media = media.to_str().unwrap();

    // --deep: the test MKVs carry no Dolby Vision configuration record
    let reports = run_detector(&["--deep", media]);
    assert_eq!(reports.len(), 14, "{reports:#?}");

    for ext in ["hevc", "mkv", "ts"] {
        assert_eq!(status_of(&reports, &format!("fel.{ext}")), "FEL");
        assert_eq!(status_of(&reports, &format!("mel.{ext}")), "MEL");
        assert_eq!(status_of(&reports, &format!("p81.{ext}")), "DOLBY_VISION");
        assert_eq!(
            status_of(&reports, &format!("sdr.{ext}")),
            "NO_DOLBY_VISION"
        );
    }
    assert_eq!(status_of(&reports, "late_fel.hevc"), "FEL");
    assert_eq!(status_of(&reports, "late_fel.ts"), "FEL");

    // Without --deep, transport streams and raw bitstreams are still inspected
    let reports = run_detector(&["--fel-only", media]);
    let mut fel: Vec<_> = reports
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    fel.sort();
    assert_eq!(fel.len(), 4, "{fel:?}");
    assert!(fel.iter().all(|p| p.contains("fel.")));

    // A single sample position only sees the MEL part of late_fel
    let reports = run_detector(&["--samples", "1", &format!("{media}/late_fel.ts")]);
    assert_eq!(status_of(&reports, "late_fel.ts"), "MEL");
}
