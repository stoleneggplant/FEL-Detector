use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use rayon::prelude::*;
use walkdir::WalkDir;

use fel_detector::detect::{self, DetectOptions, Report, Status};
use fel_detector::media::{self, SampleOptions, Tools};
use fel_detector::rpu;

const DEFAULT_EXTENSIONS: &str = "mkv,mp4,m4v,mov,ts,m2ts,mts,hevc,h265,265";

/// Detects video files containing a Dolby Vision full enhancement layer (FEL).
///
/// Profile 7 files are inspected at the RPU level to tell a full enhancement
/// layer (FEL) apart from a minimal one (MEL).
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Directories or files to scan
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Only scan the top level of each directory
    #[arg(short = 'n', long)]
    no_recursive: bool,

    /// Follow symbolic links while walking directories
    #[arg(short = 'L', long)]
    follow_links: bool,

    /// Comma separated list of file extensions to scan
    #[arg(short, long, default_value = DEFAULT_EXTENSIONS, value_delimiter = ',')]
    extensions: Vec<String>,

    /// Number of files analyzed in parallel
    #[arg(short, long, default_value_t = 4)]
    jobs: usize,

    /// Number of positions sampled across each file
    #[arg(short, long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..))]
    samples: u32,

    /// Number of frames (RPUs) read at each sample position
    #[arg(short, long, default_value_t = 48, value_parser = clap::value_parser!(u32).range(1..))]
    frames: u32,

    /// Keep sampling after FEL was found (more accurate statistics, slower)
    #[arg(long)]
    full: bool,

    /// Inspect RPUs of every HEVC file, even if the container signals no
    /// Dolby Vision enhancement layer
    #[arg(long)]
    deep: bool,

    /// Output format
    #[arg(short = 'o', long, value_enum, default_value_t = Format::Table)]
    format: Format,

    /// Only report FEL files
    #[arg(long)]
    fel_only: bool,

    /// Path to the ffmpeg executable
    #[arg(long, default_value = "ffmpeg", env = "FEL_DETECTOR_FFMPEG")]
    ffmpeg: PathBuf,

    /// Path to the ffprobe executable
    #[arg(long, default_value = "ffprobe", env = "FEL_DETECTOR_FFPROBE")]
    ffprobe: PathBuf,

    /// Disable progress output
    #[arg(short, long)]
    quiet: bool,

    /// Disable colored output
    #[arg(long)]
    no_color: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    /// Human readable table with a summary
    Table,
    Json,
    Csv,
    /// Matching file paths only, one per line
    Paths,
}

fn collect_files(cli: &Cli) -> Result<Vec<PathBuf>> {
    let extensions: Vec<String> = cli
        .extensions
        .iter()
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();

    let wanted = |path: &Path| {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.contains(&e.to_ascii_lowercase()))
    };

    let mut files = Vec::new();
    for root in &cli.paths {
        let meta =
            std::fs::metadata(root).with_context(|| format!("Cannot access {}", root.display()))?;

        if meta.is_file() {
            files.push(root.clone());
            continue;
        }

        let walker = WalkDir::new(root)
            .follow_links(cli.follow_links)
            .max_depth(if cli.no_recursive { 1 } else { usize::MAX });

        for entry in walker {
            match entry {
                Ok(e) if e.file_type().is_file() && wanted(e.path()) => files.push(e.into_path()),
                Ok(_) => {}
                Err(e) => eprintln!("warning: {e}"),
            }
        }
    }

    files.sort();
    files.dedup();
    Ok(files)
}

struct Style {
    color: bool,
}

impl Style {
    fn status(&self, status: Status, width: usize) -> String {
        let text = format!("{:<width$}", status.label());
        if !self.color {
            return text;
        }
        let code = match status {
            Status::Fel => "1;32",
            Status::Mel => "33",
            Status::DolbyVision => "36",
            Status::NoDolbyVision => "2",
            Status::Unknown => "35",
            Status::Error => "31",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

fn print_table(reports: &[Report], all: &[Report], style: &Style) {
    let profile_w = reports
        .iter()
        .map(|r| r.profile_label().len())
        .chain(["PROFILE".len()])
        .max()
        .unwrap_or(0);
    let status_w = 7;

    println!("{:<status_w$}  {:<profile_w$}  PATH", "STATUS", "PROFILE");
    for r in reports {
        println!(
            "{}  {:<profile_w$}  {}  ({})",
            style.status(r.status, status_w),
            r.profile_label(),
            r.path.display(),
            r.detail
        );
    }

    let count = |s: Status| all.iter().filter(|r| r.status == s).count();
    println!();
    println!(
        "Scanned {} files: {} FEL, {} MEL, {} other Dolby Vision, {} without Dolby Vision, {} unknown, {} errors",
        all.len(),
        count(Status::Fel),
        count(Status::Mel),
        count(Status::DolbyVision),
        count(Status::NoDolbyVision),
        count(Status::Unknown),
        count(Status::Error),
    );
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn print_csv(reports: &[Report]) {
    println!("path,status,profile,fel_rpus,mel_rpus,sampled_rpus,detail");
    for r in reports {
        let stats = r.rpu_stats.as_ref();
        let num =
            |f: fn(&rpu::RpuStats) -> u64| stats.map(|s| f(s).to_string()).unwrap_or_default();
        println!(
            "{},{},{},{},{},{},{}",
            csv_field(&r.path.to_string_lossy()),
            r.status.label(),
            r.profile_label(),
            num(|s| s.fel_rpus),
            num(|s| s.mel_rpus),
            num(|s| s.rpus),
            csv_field(&r.detail),
        );
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let files = collect_files(&cli)?;
    let show_progress = !cli.quiet && std::io::stderr().is_terminal();
    let style = Style {
        color: !cli.no_color
            && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
            && cli.format == Format::Table
            && std::io::stdout().is_terminal(),
    };

    let tools = Tools {
        ffmpeg: cli.ffmpeg.clone(),
        ffprobe: cli.ffprobe.clone(),
    };
    let needs_ffmpeg = files.iter().any(|f| !detect::is_raw_hevc(f));
    if needs_ffmpeg
        && !(media::tool_available(&tools.ffprobe) && media::tool_available(&tools.ffmpeg))
    {
        anyhow::bail!(
            "ffmpeg and ffprobe are required to analyze container files (mkv, mp4, m2ts...).\n\
             Install FFmpeg or point to the executables with --ffmpeg / --ffprobe."
        );
    }

    let opts = DetectOptions {
        tools,
        sample: SampleOptions {
            samples: cli.samples,
            frames: cli.frames,
            full: cli.full,
        },
        deep: cli.deep,
    };

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cli.jobs.max(1))
        .build()?;

    let done = AtomicUsize::new(0);
    let total = files.len();
    let reports: Vec<Report> = pool.install(|| {
        files
            .par_iter()
            .map(|f| {
                let report = detect::detect(f, &opts);
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if show_progress {
                    let mut err = std::io::stderr().lock();
                    let _ = write!(err, "\r\x1b[2K[{n}/{total}] {}", f.display());
                    let _ = err.flush();
                }
                report
            })
            .collect()
    });

    if show_progress && total > 0 {
        eprint!("\r\x1b[2K");
    }

    let shown: Vec<Report> = reports
        .iter()
        .filter(|r| !cli.fel_only || r.is_fel())
        .cloned()
        .collect();

    match cli.format {
        Format::Table => print_table(&shown, &reports, &style),
        Format::Json => println!("{}", serde_json::to_string_pretty(&shown)?),
        Format::Csv => print_csv(&shown),
        Format::Paths => {
            for r in &shown {
                println!("{}", r.path.display());
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}
