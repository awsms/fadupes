# fadupes - FindAudioDupes

`fadupes` is a CLI tool to help you find identical audio files in a music collection by scanning directories and files recursively and comparing audio characteristics.

---

## Features

- **Recursive directory scanning**
  - Accepts one or more directories and/or files as input
- **Parallel processing**
  - Uses multiple threads for faster scans
- **Progress display**
  - Global progress bar
  - Optional per-file “currently scanning” output
- **Resumable scans**
  - Optional JSON state file
  - Automatically loads existing state
  - Periodically saved during processing
  - Saved automatically on Ctrl+C
- **Symlink handling**
  - Follows symlinks by default
  - Option to ignore symlinks
  - Avoids scanning the same physical file more than once
  - Reports the resolved target path for symlinked audio files
- **Filtering options**
  - Ignore files by size (`<`, `>`, or range)
  - Skip files with a unique byte size for faster scans
- **Logging**
  - Duplicate groups written to `identical_files.log`
  - Processing errors written to `identical_files_errors.log`

---

## Building

You need Rust and Cargo installed.

```bash
cargo build --release
````

The binary will be available at:

```bash
target/release/fadupes
```

---

## Usage

### Basic usage

```bash
fadupes -i <path>
fadupes -i <directory1> <directory2>
fadupes -i <file1> <file2>
fadupes --dir <directory>
fadupes -f <pattern>
```

Inputs may be directories, files, or any combination of both.
Without `-i`, query options read the global resume state.

---

## Command-line options

* `-i, --input <PATHS...>`

  * One or more files or directories to scan

* `--dir <PATH>`

  * Query duplicate groups with at least one file under this directory
  * Returns whole duplicate groups, not only the matching file

* `-f, --find <PATTERN>`

  * Query duplicate groups with at least one file path matching this case-insensitive regex
  * Returns whole duplicate groups, not only the matching file

### Optional

* `--nolist`

  * Disable per-file list output (keeps only the global progress bar)

* `--nosym`

  * Ignore symlinks instead of following them

* `--checkpoint <N>`

  * Save the resume JSON file every `N` scanned files
  * Default: `250`

* `-t, --threads <N>`

  * Set the number of threads used for parallel scanning
  * Examples: `-t5`, `-t 5`, `--threads 5`

* `--skip-unique-size`

  * Skip files whose byte size appears only once
    (faster, but may miss duplicates)

* `--ignore-size <EXPR>`

  * Ignore files by size
  * Examples:

    * `<3MB`
    * `>800MB`
    * `3MB..800MB`

* `--state-file <PATH>`

  * Path to the resume state file
  * Default: `~/.fadupes_state.json`

* `--no-resume`

  * Disable loading and saving of the resume state

* `--format <FORMAT>`

  * Set duplicate result output format
  * Supported values: `text` (default), `json`
  * `json` writes a query-friendly duplicate report to stdout

---

## Resume behavior

* State files are written to `~/.fadupes_state.json` by default
* If the state file exists, it is loaded automatically
* The state is saved periodically during the scan (tune with `--checkpoint`)
* On Ctrl+C, the state is saved before exiting
* Query mode (`--dir` / `--find`) reads the state file without scanning

---

## Symlink behavior

By default, `fadupes` follows symlinks. When a symlink resolves to an audio file, the resolved target path is stored and reported instead of the symlink path.

During a scan, `fadupes` tracks physical files it has already seen. On Unix this uses `(device, inode)`, so hardlinks and multiple symlinks to the same file are not decoded twice. On other platforms, canonical paths are used as the fallback identity.

Use `--nosym` to ignore symlinks completely.

---

## How duplicate detection works

Each audio file is decoded and analyzed to extract audio properties.
Files are considered identical if all of the following match:

* Total sample count
* Sample rate
* Bit depth
* Channel count
* Peak level
* RMS level (dB)

Files sharing the same characteristics are grouped together as duplicates.

---

## Output

* **Console**

  * Duplicate groups are printed to stdout
  * Use `--format json` to print duplicate groups as JSON for tools like `jq`

* **Files**

  * `identical_files.log`

    * Appended with duplicate file paths (grouped)
    * Written only by default text scan output, not query output
  * `identical_files_errors.log`

    * Created only if errors occur during processing

### Query duplicate output with `jq`

Write a JSON duplicate report:

```bash
fadupes -i /music --format json > dupes.json
```

Print only duplicate track paths under one directory:

```bash
jq -r --arg dir "$(realpath /music/xyz)/" \
  '.groups[].files[] | select(.file_path | startswith($dir)) | .file_path' \
  dupes.json
```

Print every path in duplicate groups that contain at least one file under that directory:

```bash
jq -r --arg dir "$(realpath /music/xyz)/" \
  '.groups[] | select(any(.files[].file_path; startswith($dir))) | .files[].file_path' \
  dupes.json
```

### Query the global state

Print duplicate groups where any file is under a directory:

```bash
fadupes --dir /music/xyz
```

Print duplicate groups where any path matches a case-insensitive regex:

```bash
fadupes -f charrette
```

---

## Supported formats and limits

* Supported formats: **WAV**, **FLAC**
* Files larger than **800 MB** are currently skipped

---

## Development

Run in debug mode:

```bash
cargo run -- -i /path/to/music
```

---

## Current limitations

* Only WAV and FLAC are supported
* Duplicate detection is based on decoded audio characteristics, not tags
* No built-in deletion or interactive duplicate management

---

## TODO

- [ ] Additional audio formats
- [ ] Interactive duplicate handling
- [ ] Persistent audio database of all scans
