use clap::{Arg, ArgAction, Command, ValueHint, crate_version, value_parser};
use ctrlc;
use fadupes::{AudioFile, ResumeCache, SizeFilter, parse_size_filter};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

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

    compare_audio_files(&audio_files);
}

fn compare_audio_files(audio_files: &[AudioFile]) {
    let log_file_path = "identical_files.log"; // path for the log file (current dir)

    // Open the log file in append mode (creates it if not exists), currently it's a simple txt file
    let mut log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path)
        .expect("Unable to open log file");

    let mut file_map = HashMap::new();
    let mut identical_groups = Vec::new();

    // Group files by their characteristics
    for file in audio_files {
        // Use bitwise float representation so grouping is exact
        let key = (
            file.total_samples,
            file.sample_rate,
            file.bit_depth,
            file.channels,
            file.peak_level.to_bits(),
            file.rms_db_level.to_bits(),
        );

        file_map.entry(key).or_insert_with(Vec::new).push(file);
    }

    // Collect identical files into groups
    for (_, files) in &file_map {
        if files.len() > 1 {
            identical_groups.push(files);
        }
    }

    // Output the results and write to the log file
    if identical_groups.is_empty() {
        println!("Among {} files, no dupes were found.", audio_files.len());
    } else {
        let total_dupes: usize = identical_groups.iter().map(|g| g.len()).sum();
        println!("Found {} identical files:", total_dupes);

        writeln!(log_file, "Identical Files Found:").expect("Failed to write to log file");
        // Avoid logging the same dupe-group more than once in a single run (stable signature = sorted paths)
        let mut seen_groups: HashSet<Vec<String>> = HashSet::new();

        for group in identical_groups {
            // stable signature: sorted list of paths
            let mut sig: Vec<String> = group.iter().map(|f| f.file_path.clone()).collect();
            sig.sort_unstable();

            if !seen_groups.insert(sig) {
                continue; // already logged this exact set of paths in THIS run
            }

            writeln!(log_file, "#").expect("Failed to write to log file"); // Add separator for each dupe group
            for file in group {
                println!("{}", file.file_path);
                writeln!(log_file, "{}", file.file_path).expect("Failed to write to log file");
            }
            println!(); // Add an empty line between dupe groups
        }
    }
}
