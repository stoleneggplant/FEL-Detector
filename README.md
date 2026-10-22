# FEL-Detector

Scans directories for video files that contain a Dolby Vision **full enhancement layer (FEL)**.

Dolby Vision profile 7 (UHD Blu-ray) streams carry an enhancement layer (EL) next to the base
layer. The EL is either:

- **FEL**: carries real residual picture data (12-bit reconstruction), or
- **MEL**: a placeholder whose residual is a no-op.

Container metadata only says "profile 7 with an EL", so it can't tell the two apart.
FEL-Detector reads the Dolby Vision RPUs (the per-frame metadata NAL units). It then checks the NLQ
(non-linear quantization) parameters in them the same way [dovi_tool](https://github.com/quietvoid/dovi_tool) does.
It uses the same parsing library, [`dolby_vision`](https://crates.io/crates/dolby_vision).

## Requirements

- [FFmpeg](https://ffmpeg.org/) (`ffmpeg` and `ffprobe` on `PATH`) to read containers such as MKV,
  MP4 and M2TS. You don't need it for raw `.hevc` / `.h265` bitstreams.
- Rust (to build).

## Build

```sh
cargo build --release
# binary: target/release/fel-detector
```

## Usage

```sh
fel-detector /path/to/movies                  # recursive scan, table output
fel-detector --fel-only /mnt/media             # only display files with FEL (short: -F)
fel-detector -F -o paths /mnt/media            # FEL file paths only, one per line
fel-detector -o json /mnt/media > report.json  # machine readable report
fel-detector -o csv -j 8 /mnt/media            # CSV, 8 files in parallel
```

Example output:

```
STATUS   PROFILE  EL BITRATE  PATH
FEL      P7        9.8 Mb/s  /movies/Movie A (2019).mkv  (full enhancement layer, 144/144 sampled RPUs are FEL)
MEL      P7         42 kb/s  /movies/Movie B (2017).mkv  (minimal enhancement layer, 144 RPUs sampled)
DV       P8.1                /movies/Movie C (2021).mkv  (no enhancement layer signaled)
NO DV                        /movies/Movie D (2010).mkv  (no Dolby Vision configuration)

Scanned 4 files: 1 FEL, 1 MEL, 1 other Dolby Vision, 1 without Dolby Vision, 0 unknown, 0 errors
```

| Status    | Meaning                                                                    |
|-----------|----------------------------------------------------------------------------|
| `FEL`     | Dolby Vision with a full enhancement layer                                 |
| `MEL`     | Dolby Vision with a minimal enhancement layer                              |
| `DV`      | Dolby Vision without an enhancement layer (profiles 5, 8, 10...)           |
| `NO DV`   | No Dolby Vision found                                                      |
| `UNKNOWN` | Dolby Vision is signaled but no RPU could be read from the sampled frames  |
| `ERROR`   | The file couldn't be read (the error is in the detail column)              |

### Options

| Option                    | Default                                  | Description                                                                 |
|---------------------------|------------------------------------------|-----------------------------------------------------------------------------|
| `-o, --format`            | `table`                                  | `table`, `json`, `csv` or `paths`                                           |
| `-F, --fel-only`          |                                          | Only display files with FEL (the summary line still counts every file)     |
| `-e, --extensions`        | `mkv,mp4,m4v,mov,ts,m2ts,mts,hevc,h265,265` | File extensions to scan                                                  |
| `-n, --no-recursive`      |                                          | Don't descend into subdirectories                                           |
| `-L, --follow-links`      |                                          | Follow symbolic links                                                       |
| `-j, --jobs`              | `4`                                      | Files analyzed in parallel                                                  |
| `-s, --samples`           | `3`                                      | Positions sampled across each file (start, 1/3, 2/3)                        |
| `-f, --frames`            | `48`                                     | Frames (RPUs) read at each position                                         |
| `--fast`                  |                                          | Skip the remaining sample positions once FEL was found (rougher EL bitrate) |
| `--deep`                  |                                          | Inspect the RPUs of every HEVC file, even if the container signals no EL   |
| `--ffmpeg`, `--ffprobe`   | `ffmpeg`, `ffprobe`                      | Executable paths (also `FEL_DETECTOR_FFMPEG` / `FEL_DETECTOR_FFPROBE`)      |
| `-q, --quiet`             |                                          | No progress output                                                          |
| `--no-color`              |                                          | No colors (`NO_COLOR` is respected too)                                     |

## How it works

1. `ffprobe` reads the Dolby Vision configuration record (the `dvcC`/`dvvC` box or MKV block
   addition mapping) of each video stream.
2. If the file is profile 7 or signals an enhancement layer, `ffmpeg` remuxes a few short
   segments (`--samples` × `--frames`) of the HEVC stream to an Annex B bitstream. Only the
   container is unpacked; nothing is decoded. FEL-Detector reads that bitstream from a pipe.
3. Every `UNSPEC62` NAL unit (RPU) gets parsed. An RPU counts as **MEL** when its NLQ
   parameters are a no-op: zero offset, `vdr_in_max` = 1.0, zero deadzone slope and threshold.
   Any other RPU with NLQ data counts as **FEL**. A file is reported as FEL as soon as one
   sampled RPU is FEL.
4. The **EL bitrate** is estimated from the same sampled frames: EL bytes per frame × frame rate.
   - Single-track files (MKV/MP4 profile 7): the EL bytes are the `UNSPEC63` NAL units that wrap
     the enhancement layer.
   - Blu-ray dual-PID streams: the EL bytes are the whole EL stream minus the RPUs.

   The estimate only covers the sampled windows (3 × 48 frames by default). Raise `--samples`
   for a figure closer to the file's average. The frame rate comes from ffprobe; raw `.hevc`
   files use the VUI timing. JSON and CSV report it as `el_bitrate_kbps`. Files without an EL
   leave it empty.

Transport streams (`.ts`, `.m2ts`, `.mts`) and raw HEVC files always get their RPUs inspected,
because ffprobe often doesn't expose the Dolby Vision configuration for them. Blu-ray dual-PID
streams work too: the RPUs come from the enhancement layer PID.

## Tests

```sh
cargo test
```

The end-to-end test needs `ffmpeg` with `libx265`; without it, the test is skipped. It encodes
small clips, adds generated FEL, MEL and profile 8.1 RPUs, muxes them into MKV and TS, and then
runs the binary on the resulting directory. It also builds single-track and dual-PID dual-layer
files from a second encoded clip and checks the measured EL bitrate against the clip's real
size.
