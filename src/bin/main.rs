use clap::{Arg, ArgAction, Command, ValueHint, crate_version, value_parser};
use ctrlc;
use fadupes::{AudioFile, CachedEntry, ResumeCache, SeenFiles, SizeFilter, parse_size_filter};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
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

struct Query {
    dir: Option<PathBuf>,
    pattern: Option<Regex>,
}

impl Query {
    fn is_active(&self) -> bool {
        self.dir.is_some() || self.pattern.is_some()
    }

    fn matches(&self, file: &AudioFile) -> bool {
        let path = Path::new(&file.file_path);
        let dir_matches = self.dir.as_ref().is_none_or(|dir| path.starts_with(dir));
        let pattern_matches = self
            .pattern
            .as_ref()
            .is_none_or(|pattern| pattern.is_match(&file.file_path));

        dir_matches && pattern_matches
    }
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
                .num_args(1..)
                .value_hint(ValueHint::FilePath)
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("dir")
                .long("dir")
                .value_name("PATH")
                .help("Query duplicate groups with at least one file under this directory")
                .value_hint(ValueHint::DirPath)
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("find")
                .short('f')
                .long("find")
                .value_name("PATTERN")
                .help("Query duplicate groups with at least one file path matching this case-insensitive regex"),
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
                .help("Path to the resume state file (default: ~/.fadupes_state.json)")
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
            Arg::new("cleanup")
                .long("cleanup")
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["dir", "find", "no_resume"])
                .help("Remove state entries for files that no longer exist, then exit"),
        )
        .arg(
            Arg::new("dry_run")
                .long("dry-run")
                .action(ArgAction::SetTrue)
                .requires("cleanup")
                .help("Show how many state entries --cleanup would remove without writing the state file"),
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

    let output_format = match matches
        .get_one::<String>("format")
        .map(String::as_str)
        .unwrap_or("text")
    {
        "json" => OutputFormat::Json,
        _ => OutputFormat::Text,
    };
    let inputs: Vec<PathBuf> = matches
        .get_many::<PathBuf>("input")
        .map(|paths| paths.cloned().collect())
        .unwrap_or_default();
    let query = build_query(
        matches.get_one::<PathBuf>("dir").cloned(),
        matches.get_one::<String>("find").cloned(),
    );
    let cleanup = matches.get_flag("cleanup");
    let dry_run = matches.get_flag("dry_run");

    let list_files = !matches.get_flag("nolist") && !matches!(output_format, OutputFormat::Json);
    let skip_unique_size = matches.get_flag("skip_unique_size");
    let ignore_symlinks = matches.get_flag("nosym");
    let no_resume = matches.get_flag("no_resume");
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
    let state_file = provided_state_file.unwrap_or_else(default_state_file);
    if cleanup {
        cleanup_state_file(&state_file, &inputs, checkpoint, dry_run);
        return;
    }

    if inputs.is_empty() && !query.is_active() {
        eprintln!("Either --input/-i, --dir, --find/-f, or --cleanup is required");
        std::process::exit(2);
    }

    let resume_cache = if resume_enabled && !inputs.is_empty() {
        Some(Arc::new(ResumeCache::load(state_file.clone(), checkpoint)))
    } else {
        None
    };

    // If resume is enabled, trap Ctrl+C so we can persist the cache before exiting (130 = SIGINT)
    if !inputs.is_empty()
        && let Some(cache) = resume_cache.as_ref()
    {
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

    let write_log = !inputs.is_empty() && !query.is_active();
    let audio_files = if inputs.is_empty() {
        load_audio_files_from_state(&state_file)
    } else {
        scan_audio_files(
            inputs,
            list_files,
            skip_unique_size,
            ignore_symlinks,
            resume_cache,
            ignore_size.as_ref(),
        )
    };

    compare_audio_files(&audio_files, output_format, &query, write_log);
}

fn default_state_file() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".fadupes_state.json"))
        .unwrap_or_else(|| PathBuf::from("fadupes_state.json"))
}

fn cleanup_state_file(state_file: &Path, inputs: &[PathBuf], checkpoint: usize, dry_run: bool) {
    if !state_file.exists() {
        println!(
            "State file not found: {}. Checked 0 state entries.",
            state_file.display()
        );
        return;
    }

    let roots = cleanup_roots(inputs);
    let cache = if dry_run {
        ResumeCache::load_read_only(state_file.to_path_buf(), checkpoint)
    } else {
        ResumeCache::load(state_file.to_path_buf(), checkpoint)
    };
    let report = cache
        .cleanup_missing(&roots, dry_run)
        .unwrap_or_else(|err| {
            eprintln!(
                "Failed to cleanup state file {}: {err}",
                state_file.display()
            );
            std::process::exit(1);
        });
    let scope = if roots.is_empty() {
        "all state entries".to_string()
    } else {
        format!("state entries under {} input root(s)", roots.len())
    };

    if dry_run {
        println!(
            "Dry run: would remove {} stale state entries from {} (checked {}). No files would be deleted.",
            report.stale_entries, scope, report.checked_entries
        );
    } else {
        println!(
            "Removed {} stale state entries from {} (checked {}). No audio files were deleted.",
            report.stale_entries, scope, report.checked_entries
        );
    }
}

fn cleanup_roots(inputs: &[PathBuf]) -> Vec<PathBuf> {
    inputs
        .iter()
        .map(|input| {
            std::fs::canonicalize(input).unwrap_or_else(|_| {
                if input.is_absolute() {
                    input.clone()
                } else {
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .join(input)
                }
            })
        })
        .collect()
}

fn build_query(dir: Option<PathBuf>, pattern: Option<String>) -> Query {
    let dir = dir.map(|dir| std::fs::canonicalize(&dir).unwrap_or(dir));
    let pattern = pattern.map(|pattern| {
        RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .build()
            .unwrap_or_else(|err| {
                eprintln!("--find regex parse error: {err}");
                std::process::exit(2);
            })
    });

    Query { dir, pattern }
}

fn load_audio_files_from_state(path: &Path) -> Vec<AudioFile> {
    let file = std::fs::File::open(path).unwrap_or_else(|err| {
        eprintln!("Failed to open state file {}: {err}", path.display());
        std::process::exit(1);
    });
    let entries: HashMap<String, CachedEntry> =
        serde_json::from_reader(file).unwrap_or_else(|err| {
            eprintln!("Failed to parse state file {}: {err}", path.display());
            std::process::exit(1);
        });

    entries
        .into_iter()
        .map(|(file_path, entry)| entry.to_audio_file(file_path))
        .collect()
}

fn scan_audio_files(
    inputs: Vec<PathBuf>,
    list_files: bool,
    skip_unique_size: bool,
    ignore_symlinks: bool,
    resume_cache: Option<Arc<ResumeCache>>,
    ignore_size: Option<&SizeFilter>,
) -> Vec<AudioFile> {
    // Create a HashSet of scanned directories to pass to the walk_dir function
    let mut unique_inputs = Vec::new();
    let mut scanned_dirs = HashSet::new();
    for input in inputs {
        let full_path = std::fs::canonicalize(&input).unwrap_or_else(|e| {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        });
        if scanned_dirs.insert(full_path.clone()) {
            unique_inputs.push(full_path);
        }
    }

    let seen_files = SeenFiles::new();

    // Collect all the audio files from all inputs
    unique_inputs
        .into_par_iter() // Process directories in parallel
        .flat_map(|full_path| {
            AudioFile::walk_dir(
                &full_path,
                &scanned_dirs,
                list_files,
                skip_unique_size,
                ignore_symlinks,
                resume_cache.clone(),
                ignore_size,
                seen_files.clone(),
            )
            .into_par_iter()
        })
        .collect()
}

fn compare_audio_files(
    audio_files: &[AudioFile],
    output_format: OutputFormat,
    query: &Query,
    write_log: bool,
) {
    let groups = filter_duplicate_groups(collect_duplicate_groups(audio_files), query);

    match output_format {
        OutputFormat::Text => write_text_output(audio_files.len(), &groups, write_log),
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

fn filter_duplicate_groups<'a>(
    groups: Vec<DuplicateGroup<'a>>,
    query: &Query,
) -> Vec<DuplicateGroup<'a>> {
    if !query.is_active() {
        return groups;
    }

    groups
        .into_iter()
        .filter(|group| group.files.iter().any(|file| query.matches(file)))
        .collect()
}

fn write_text_output(
    scanned_files: usize,
    duplicate_groups: &[DuplicateGroup<'_>],
    write_log: bool,
) {
    // Output the results and write to the log file
    if duplicate_groups.is_empty() {
        println!("Among {} files, no dupes were found.", scanned_files);
    } else {
        let total_dupes: usize = duplicate_groups.iter().map(|g| g.files.len()).sum();
        println!("Found {} identical files:", total_dupes);

        let mut log_file = if write_log {
            let log_file_path = "identical_files.log"; // path for the log file (current dir)
            Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_file_path)
                    .expect("Unable to open log file"),
            )
        } else {
            None
        };
        if let Some(log_file) = log_file.as_mut() {
            writeln!(log_file, "Identical Files Found:").expect("Failed to write to log file");
        }

        // Avoid logging the same dupe-group more than once in a single run (stable signature = sorted paths)
        let mut seen_groups: HashSet<Vec<String>> = HashSet::new();

        for group in duplicate_groups {
            // stable signature: sorted list of paths
            let sig: Vec<String> = group.files.iter().map(|f| f.file_path.clone()).collect();

            if !seen_groups.insert(sig) {
                continue; // already logged this exact set of paths in THIS run
            }

            if let Some(log_file) = log_file.as_mut() {
                writeln!(log_file, "#").expect("Failed to write to log file"); // Add separator for each dupe group
            }
            for file in &group.files {
                println!("{}", file.file_path);
                if let Some(log_file) = log_file.as_mut() {
                    writeln!(log_file, "{}", file.file_path).expect("Failed to write to log file");
                }
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
