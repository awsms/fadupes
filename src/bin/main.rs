use clap::{Arg, ArgAction, Command, ValueHint, crate_version, value_parser};
use ctrlc;
use fadupes::{AudioFile, ResumeCache, SizeFilter, parse_size_filter};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Copy)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Serialize)]
struct DuplicateSignature {
    total_samples: u64,
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
    peak_level_bits: u32,
    rms_db_level_bits: u64,
}

impl DuplicateSignature {
    fn from_audio_file(file: &AudioFile) -> Self {
        Self {
            total_samples: file.total_samples,
            sample_rate: file.sample_rate,
            bit_depth: file.bit_depth,
            channels: file.channels,
            peak_level_bits: file.peak_level.to_bits(),
            rms_db_level_bits: file.rms_db_level.to_bits(),
        }
    }

    fn id(&self) -> String {
        format!(
            "{}:{}:{}:{}:{:08x}:{:016x}",
            self.total_samples,
            self.sample_rate,
            self.bit_depth,
            self.channels,
            self.peak_level_bits,
            self.rms_db_level_bits
        )
    }
}

#[derive(Serialize)]
struct DuplicateGroup<'a> {
    id: String,
    signature: DuplicateSignature,
    files: Vec<&'a AudioFile>,
}

#[derive(Serialize)]
struct DuplicateReport<'a> {
    schema_version: u32,
    tool: ToolInfo<'a>,
    summary: DuplicateSummary,
    groups: Vec<DuplicateGroup<'a>>,
}

#[derive(Serialize)]
struct ToolInfo<'a> {
    name: &'a str,
    version: &'a str,
}

#[derive(Serialize)]
struct DuplicateSummary {
    scanned_files: usize,
    duplicate_groups: usize,
    duplicate_files: usize,
}

fn main() {
    let matches = Command::new("Audio dupechecker")
        .version(crate_version!())
        .author("menfou")
        .about("Compares audio files in a given directory or multiple inputs and identifies identical files")
        .arg(
            Arg::new("input")
                .short('i')
                .long("input")
                .help("Sets the directory to scan for audio files")
                .required(true)
                .num_args(1..)
                .value_hint(ValueHint::FilePath)
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("skip_unique_size")
                .long("skip-unique-size")
                .action(ArgAction::SetTrue)
                .help("Skip files whose byte size is unique (faster, but may miss dupes)"),
        )
        .arg(
            Arg::new("nolist")
                .long("nolist")
                .action(ArgAction::SetTrue)
                .help("Disable showing the file list as files are scanned"),
        )
        .arg(
            Arg::new("state_file")
                .long("state-file")
                .value_hint(ValueHint::FilePath)
                .help("Path to the resume state file (default: fadupes_state.json)")
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("nosym")
                .long("nosym")
                .action(ArgAction::SetTrue)
                .help("Ignore symlinks instead of following them"),
        )
        .arg(
            Arg::new("no_resume")
                .long("no-resume")
                .action(ArgAction::SetTrue)
                .help("Disable resuming from / saving to the state file"),
        )
        .arg(
            Arg::new("ignore_size")
                .long("ignore-size")
                .value_name("EXPR")
                .help(r#"Ignore files by size. Examples: "<3MB", ">800MB", "3MB..800MB""#),
        )
        .arg(
            Arg::new("checkpoint")
                .long("checkpoint")
                .value_name("N")
                .help("Save the resume JSON every N scanned files")
                .default_value("250")
                .value_parser(value_parser!(usize)),
        )
        .arg(
            Arg::new("threads")
                .short('t')
                .long("threads")
                .value_name("N")
                .help("Set number of threads used for parallel scanning (default: Rayon default)")
                .value_parser(value_parser!(usize)),
        )
        .arg(
            Arg::new("format")
                .long("format")
                .value_name("FORMAT")
                .help("Set duplicate result output format")
                .default_value("text")
                .value_parser(["text", "json"]),
        )
        .get_matches();

    let threads = matches.get_one::<usize>("threads").copied();
    if let Some(threads) = threads {
        if threads == 0 {
            eprintln!("--threads must be at least 1");
            std::process::exit(2);
        }
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .unwrap_or_else(|e| {
                eprintln!("Failed to configure Rayon thread pool: {e}");
                std::process::exit(2);
            });
    }

    let inputs: Vec<PathBuf> = matches
        .get_many::<PathBuf>("input")
        .unwrap()
        .cloned()
        .collect();
    let list_files = !matches.get_flag("nolist");
    let skip_unique_size = matches.get_flag("skip_unique_size");
    let ignore_symlinks = matches.get_flag("nosym");
    let no_resume = matches.get_flag("no_resume");
    let output_format = match matches
        .get_one::<String>("format")
        .map(String::as_str)
        .unwrap_or("text")
    {
        "json" => OutputFormat::Json,
        _ => OutputFormat::Text,
    };
    let ignore_size_expr = matches.get_one::<String>("ignore_size").cloned();
    let ignore_size: Option<SizeFilter> = ignore_size_expr
        .as_deref()
        .map(parse_size_filter)
        .transpose()
        .unwrap_or_else(|e| {
            eprintln!("--ignore-size parse error: {e}");
            std::process::exit(2);
        });
    let checkpoint = *matches
        .get_one::<usize>("checkpoint")
        .expect("defaulted above");
    if checkpoint == 0 {
        eprintln!("--checkpoint must be at least 1");
        std::process::exit(2);
    }
    let provided_state_file = matches.get_one::<PathBuf>("state_file").cloned();
    let resume_enabled = !no_resume;
    let state_file = provided_state_file.unwrap_or_else(|| PathBuf::from("fadupes_state.json"));
    let resume_cache = if resume_enabled {
        Some(Arc::new(ResumeCache::load(state_file, checkpoint)))
    } else {
        None
    };

    // If resume is enabled, trap Ctrl+C so we can persist the cache before exiting (130 = SIGINT)
    if let Some(cache) = resume_cache.as_ref() {
        let cache_for_signal = Arc::clone(cache);
        ctrlc::set_handler(move || {
            let _ = cache_for_signal.save();
            eprintln!(
                "\nSaved resume state to {}",
                cache_for_signal.path().display()
            );
            std::process::exit(130);
        })
        .expect("Error setting Ctrl+C handler");
    }

    // Create a HashSet of scanned directories to pass to the walk_dir function
    let scanned_dirs: HashSet<PathBuf> = inputs.iter().cloned().collect();

    // Collect all the audio files from all inputs
    let audio_files: Vec<AudioFile> = inputs
        .into_par_iter() // Process directories in parallel
        .flat_map(|input| {
            let full_path = std::fs::canonicalize(&input).unwrap_or_else(|e| {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            });

            AudioFile::walk_dir(
                &full_path,
                &scanned_dirs,
                list_files,
                skip_unique_size,
                ignore_symlinks,
                resume_cache.clone(),
                ignore_size.as_ref(),
            )
            .into_par_iter()
        })
        .collect();

    compare_audio_files(&audio_files, output_format);
}

fn compare_audio_files(audio_files: &[AudioFile], output_format: OutputFormat) {
    let groups = collect_duplicate_groups(audio_files);

    match output_format {
        OutputFormat::Text => write_text_output(audio_files.len(), &groups),
        OutputFormat::Json => write_json_output(audio_files.len(), groups),
    }
}

fn collect_duplicate_groups(audio_files: &[AudioFile]) -> Vec<DuplicateGroup<'_>> {
    let mut file_map: HashMap<DuplicateSignature, Vec<&AudioFile>> = HashMap::new();

    for file in audio_files {
        let key = DuplicateSignature::from_audio_file(file);
        file_map.entry(key).or_default().push(file);
    }

    let mut groups: Vec<_> = file_map
        .into_iter()
        .filter_map(|(signature, mut files)| {
            if files.len() <= 1 {
                return None;
            }

            files.sort_unstable_by(|a, b| a.file_path.cmp(&b.file_path));
            Some(DuplicateGroup {
                id: signature.id(),
                signature,
                files,
            })
        })
        .collect();

    groups.sort_unstable_by(|a, b| a.files[0].file_path.cmp(&b.files[0].file_path));
    groups
}

fn write_text_output(scanned_files: usize, duplicate_groups: &[DuplicateGroup<'_>]) {
    let log_file_path = "identical_files.log"; // path for the log file (current dir)

    // Open the log file in append mode (creates it if not exists), currently it's a simple txt file
    let mut log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path)
        .expect("Unable to open log file");

    // Output the results and write to the log file
    if duplicate_groups.is_empty() {
        println!("Among {} files, no dupes were found.", scanned_files);
    } else {
        let total_dupes: usize = duplicate_groups.iter().map(|g| g.files.len()).sum();
        println!("Found {} identical files:", total_dupes);

        writeln!(log_file, "Identical Files Found:").expect("Failed to write to log file");
        // Avoid logging the same dupe-group more than once in a single run (stable signature = sorted paths)
        let mut seen_groups: HashSet<Vec<String>> = HashSet::new();

        for group in duplicate_groups {
            // stable signature: sorted list of paths
            let sig: Vec<String> = group.files.iter().map(|f| f.file_path.clone()).collect();

            if !seen_groups.insert(sig) {
                continue; // already logged this exact set of paths in THIS run
            }

            writeln!(log_file, "#").expect("Failed to write to log file"); // Add separator for each dupe group
            for file in &group.files {
                println!("{}", file.file_path);
                writeln!(log_file, "{}", file.file_path).expect("Failed to write to log file");
            }
            println!(); // Add an empty line between dupe groups
        }
    }
}

fn write_json_output(scanned_files: usize, groups: Vec<DuplicateGroup<'_>>) {
    let duplicate_files = groups.iter().map(|group| group.files.len()).sum();
    let report = DuplicateReport {
        schema_version: 1,
        tool: ToolInfo {
            name: "fadupes",
            version: crate_version!(),
        },
        summary: DuplicateSummary {
            scanned_files,
            duplicate_groups: groups.len(),
            duplicate_files,
        },
        groups,
    };

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer_pretty(&mut stdout, &report).expect("Failed to write JSON output");
    writeln!(stdout).expect("Failed to write JSON output");
}
