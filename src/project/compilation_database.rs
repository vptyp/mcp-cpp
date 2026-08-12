use json_compilation_db::Entry;
use serde::{Deserialize, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Type alias for bidirectional path mappings
/// (original_path -> canonical_path, canonical_path -> original_path)
pub type PathMappings = (HashMap<PathBuf, PathBuf>, HashMap<PathBuf, PathBuf>);

/// Aggregated compiler options extracted from a compilation database.
///
/// These are derived from the `arguments` field of compilation entries and
/// aggregated across a bounded sample of entries (see
/// [`CompilationDatabase::aggregate_build_options_from_path`]).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuildOptions {
    /// Unique preprocessor defines (`-DNAME=VALUE`)
    pub defines: Vec<String>,
    /// Unique include paths (`-I/path`, `-isystem /path`)
    pub include_paths: Vec<String>,
    /// Most common language standard (`-std=...`)
    pub language_standard: Option<String>,
    /// Most common optimization level (`-O0`..`-O3`, `-Os`)
    pub optimization: Option<String>,
    /// Other unique compiler flags (e.g. `-fPIC`, `-Wall`)
    pub flags: Vec<String>,
    /// Number of compilation entries sampled to build this summary
    pub entries_sampled: usize,
}

/// Default number of compilation entries to sample when aggregating build options.
///
/// A bounded sample keeps the aggregation fast (streaming, reads only the head of
/// the file) and representative, without loading a potentially huge database.
pub const DEFAULT_BUILD_OPTIONS_SAMPLE: usize = 200;

#[derive(Error, Debug)]
pub enum CompilationDatabaseError {
    #[error("Compilation database file not found: {path}")]
    FileNotFound { path: String },
    #[error("Failed to read compilation database file: {error}")]
    ReadError { error: String },
    #[error("Failed to parse compilation database JSON: {error}")]
    ParseError { error: String },
    #[error("Compilation database is empty")]
    EmptyDatabase,
}

/// Wrapper around compilation database providing structured access to compilation entries
///
/// This struct contains both the path to the compilation database file and the parsed entries.
/// When serialized, only the path is included in the output to avoid serializing large database content.
#[derive(Debug, Clone, Deserialize)]
pub struct CompilationDatabase {
    /// Path to the compilation database file (compile_commands.json)
    pub path: PathBuf,
    /// Parsed compilation database entries (loaded at initialization)
    #[serde(skip)]
    pub entries: Vec<Entry>,
}

impl CompilationDatabase {
    /// Create a new compilation database by loading and parsing the file at the given path
    ///
    /// This immediately loads and parses the compilation database, returning an error if
    /// the file doesn't exist, can't be read, or contains invalid JSON.
    pub fn new(path: PathBuf) -> Result<Self, CompilationDatabaseError> {
        // Check if file exists
        if !path.exists() {
            return Err(CompilationDatabaseError::FileNotFound {
                path: path.to_string_lossy().to_string(),
            });
        }

        // Open and read the file
        let file = std::fs::File::open(&path).map_err(|e| CompilationDatabaseError::ReadError {
            error: e.to_string(),
        })?;

        // Parse the JSON compilation database
        let reader = std::io::BufReader::new(file);
        let entries: Vec<Entry> =
            serde_json::from_reader(reader).map_err(|e| CompilationDatabaseError::ParseError {
                error: e.to_string(),
            })?;

        // Check if database is empty
        if entries.is_empty() {
            return Err(CompilationDatabaseError::EmptyDatabase);
        }

        Ok(Self { path, entries })
    }

    /// Create a compilation database from entries for testing
    ///
    /// This bypasses filesystem operations and creates a CompilationDatabase
    /// directly from provided entries, useful for unit tests.
    #[cfg(test)]
    pub fn from_entries(entries: Vec<Entry>) -> Self {
        Self {
            path: PathBuf::from("/test/compile_commands.json"),
            entries,
        }
    }

    /// Get all compilation database entries
    #[allow(dead_code)]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Get the path to the compilation database file
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Get all unique source files with canonicalized paths
    ///
    /// This method resolves relative paths against the compilation database directory
    /// and canonicalizes them to absolute paths. This ensures consistent path handling
    /// between CMake (which uses absolute paths) and Meson (which uses relative paths).
    pub fn canonical_source_files(&self) -> Result<Vec<PathBuf>, CompilationDatabaseError> {
        let mut canonical_files = Vec::new();
        let mut seen_files = std::collections::HashSet::new();

        for entry in &self.entries {
            let canonical_path = self.canonicalize_entry_path(&entry.file)?;
            if seen_files.insert(canonical_path.clone()) {
                canonical_files.push(canonical_path);
            }
        }

        canonical_files.sort();
        Ok(canonical_files)
    }

    /// Aggregate compiler options from a compilation database file.
    ///
    /// Streams the JSON file and parses only the first `max_entries` entries, so it
    /// stays fast and memory-bounded even for very large databases (e.g. Chromium's
    /// 553 MB compile_commands.json). Returns an empty [`BuildOptions`] if the file
    /// can't be read or contains no parseable entries.
    pub fn aggregate_build_options_from_path(
        path: &Path,
        max_entries: usize,
    ) -> Result<BuildOptions, CompilationDatabaseError> {
        let file = std::fs::File::open(path).map_err(|e| CompilationDatabaseError::ReadError {
            error: e.to_string(),
        })?;
        let reader = std::io::BufReader::new(file);

        let mut defines = HashSet::new();
        let mut include_paths = HashSet::new();
        let mut flags = HashSet::new();
        let mut std_counts: HashMap<String, usize> = HashMap::new();
        let mut opt_counts: HashMap<String, usize> = HashMap::new();
        let mut entries_sampled = 0usize;

        // `json_compilation_db::read` streams a JSON array of entries, so we only
        // parse the head of the file and stop after `max_entries`.
        for entry in json_compilation_db::read(reader) {
            if entries_sampled >= max_entries {
                break;
            }
            let entry = entry.map_err(|e| CompilationDatabaseError::ParseError {
                error: e.to_string(),
            })?;
            entries_sampled += 1;

            let mut args = entry.arguments.iter();
            while let Some(arg) = args.next() {
                if let Some(def) = arg.strip_prefix("-D") {
                    defines.insert(def.to_string());
                } else if let Some(inc) = arg.strip_prefix("-I") {
                    include_paths.insert(inc.to_string());
                } else if arg == "-isystem" {
                    if let Some(inc) = args.next() {
                        include_paths.insert(inc.to_string());
                    }
                } else if let Some(std) = arg.strip_prefix("-std=") {
                    *std_counts.entry(std.to_string()).or_insert(0) += 1;
                } else if arg.starts_with("-O") && arg.len() <= 3 {
                    *opt_counts.entry(arg.clone()).or_insert(0) += 1;
                } else if arg == "-c" {
                    // compile-only, not a build option
                } else if arg == "-o" {
                    // output-file flag; skip its separate argument too
                    let _ = args.next();
                } else if arg.starts_with("-o") && arg.len() > 2 {
                    // -o<file>, not a build option
                } else if arg.starts_with('-') {
                    flags.insert(arg.to_string());
                }
            }
        }

        Ok(BuildOptions {
            defines: sorted_unique(defines),
            include_paths: sorted_unique(include_paths),
            language_standard: most_common(&std_counts),
            optimization: most_common(&opt_counts),
            flags: sorted_unique(flags),
            entries_sampled,
        })
    }

    /// Derive the source root directory from the compilation database entries
    ///
    /// Computes the common ancestor directory of all source file paths in the
    /// database. This is used to determine the project source root for build
    /// systems that don't provide explicit project metadata (e.g., GN, Bazel,
    /// xmake) but still export a compile_commands.json.
    pub fn derive_source_root(&self) -> Result<PathBuf, CompilationDatabaseError> {
        let files = self.canonical_source_files()?;
        if files.is_empty() {
            return Err(CompilationDatabaseError::EmptyDatabase);
        }

        // Compute the common ancestor directory of all source files. For a single
        // file the common ancestor is the file itself, so use its parent directory
        // to ensure we always return a directory (the source root).
        let mut common = files[0].clone();
        if files.len() == 1 {
            common = files[0]
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| files[0].clone());
        } else {
            for file in &files[1..] {
                common = common_ancestor(&common, file);
            }
        }
        Ok(common)
    }

    /// Get bidirectional mappings between original and canonical paths
    ///
    /// Returns (original -> canonical, canonical -> original) mappings.
    /// This enables efficient lookup in both directions without repeated canonicalization.
    pub fn path_mappings(&self) -> Result<PathMappings, CompilationDatabaseError> {
        let mut original_to_canonical = HashMap::new();
        let mut canonical_to_original = HashMap::new();

        for entry in &self.entries {
            let original_path = entry.file.clone();
            let canonical_path = self.canonicalize_entry_path(&entry.file)?;

            original_to_canonical.insert(original_path.clone(), canonical_path.clone());
            canonical_to_original.insert(canonical_path, original_path);
        }

        Ok((original_to_canonical, canonical_to_original))
    }

    /// Canonicalize a single entry path using the same logic for all paths
    ///
    /// This is the single source of truth for path canonicalization in the system.
    /// It handles both CMake's absolute paths and Meson's relative paths consistently.
    fn canonicalize_entry_path(
        &self,
        entry_path: &Path,
    ) -> Result<PathBuf, CompilationDatabaseError> {
        let compilation_db_dir = self.path.parent().unwrap_or_else(|| Path::new("."));

        // Resolve relative paths against compilation database directory
        let resolved_path = if entry_path.is_relative() {
            compilation_db_dir.join(entry_path)
        } else {
            entry_path.to_path_buf()
        };

        // Attempt canonicalization, fall back to resolved path if it fails
        // This handles cases where files don't exist yet (like in tests)
        match resolved_path.canonicalize() {
            Ok(canonical) => Ok(canonical),
            Err(_) => {
                // For non-existent files (tests, etc.), use the resolved path
                Ok(resolved_path)
            }
        }
    }
}

/// Compute the longest common directory prefix of two paths
///
/// Returns the deepest directory that is an ancestor of both paths. Used to
/// derive a project source root from a set of source file paths.
fn common_ancestor(a: &Path, b: &Path) -> PathBuf {
    let a_components: Vec<_> = a.components().collect();
    let b_components: Vec<_> = b.components().collect();

    let mut result = PathBuf::new();
    for (x, y) in a_components.iter().zip(b_components.iter()) {
        if x == y {
            result.push(x.as_os_str());
        } else {
            break;
        }
    }
    result
}

/// Sort a set of strings into a stable `Vec<String>`.
fn sorted_unique(set: HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    v
}

/// Return the key with the highest count, if any.
fn most_common(counts: &HashMap<String, usize>) -> Option<String> {
    counts
        .iter()
        .max_by_key(|(_, c)| *c)
        .map(|(k, _)| k.clone())
}

/// Custom serialization that only outputs the path field
///
/// This ensures that when the CompilationDatabase is serialized (e.g., in JSON responses),
/// only the path is included, not the potentially large entries array.
impl Serialize for CompilationDatabase {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.path.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use json_compilation_db::Entry;

    fn entry(file: &str, directory: &str) -> Entry {
        Entry {
            file: PathBuf::from(file),
            arguments: vec![],
            directory: PathBuf::from(directory),
            output: None,
        }
    }

    #[test]
    fn test_derive_source_root_common_ancestor() {
        let db = CompilationDatabase::from_entries(vec![
            entry("/proj/src/a.cpp", "/proj/build"),
            entry("/proj/src/b.cpp", "/proj/build"),
            entry("/proj/src/sub/c.cpp", "/proj/build"),
        ]);
        let root = db.derive_source_root().unwrap();
        assert_eq!(root, PathBuf::from("/proj/src"));
    }

    #[test]
    fn test_derive_source_root_single_file() {
        let db = CompilationDatabase::from_entries(vec![entry("/proj/src/a.cpp", "/proj/build")]);
        let root = db.derive_source_root().unwrap();
        assert_eq!(root, PathBuf::from("/proj/src"));
    }

    #[test]
    fn test_derive_source_root_empty_database() {
        let db = CompilationDatabase::from_entries(vec![]);
        assert!(db.derive_source_root().is_err());
    }

    #[test]
    fn test_common_ancestor_disjoint_paths() {
        // Disjoint paths share only the root
        let a = Path::new("/proj/src/a.cpp");
        let b = Path::new("/other/lib/b.cpp");
        assert_eq!(common_ancestor(a, b), PathBuf::from("/"));
    }

    #[test]
    fn test_aggregate_build_options_from_path() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("mcp_cc_agg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("compile_commands.json");
        let json = r#"[
            {"file":"a.cpp","directory":"/proj","arguments":["c++","-std=c++17","-O2","-DNDEBUG","-Iinclude","-fPIC","-c","a.cpp"]},
            {"file":"b.cpp","directory":"/proj","arguments":["c++","-std=c++17","-O2","-DUSE_AURA","-isystem","/sys/include","-Wall","-c","b.cpp"]},
            {"file":"c.cpp","directory":"/proj","arguments":["c++","-std=c++20","-O0","-DNDEBUG","-Iinclude","-c","c.cpp"]}
        ]"#;
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f.flush().unwrap();

        let opts = CompilationDatabase::aggregate_build_options_from_path(&path, 200).unwrap();

        assert_eq!(opts.entries_sampled, 3);
        assert_eq!(opts.defines, vec!["NDEBUG", "USE_AURA"]);
        assert_eq!(opts.include_paths, vec!["/sys/include", "include"]);
        assert_eq!(opts.language_standard.as_deref(), Some("c++17"));
        assert_eq!(opts.optimization.as_deref(), Some("-O2"));
        assert!(opts.flags.contains(&"-fPIC".to_string()));
        assert!(opts.flags.contains(&"-Wall".to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_aggregate_build_options_respects_max_entries() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("mcp_cc_agg2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("compile_commands.json");
        let mut entries = Vec::new();
        for i in 0..10 {
            entries.push(format!(
                "{{\"file\":\"f{}.cpp\",\"directory\":\"/proj\",\"arguments\":[\"c++\",\"-DOPT{}\",\"-c\",\"f{}.cpp\"]}}",
                i, i, i
            ));
        }
        let json = format!("[{}]", entries.join(","));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f.flush().unwrap();

        let opts = CompilationDatabase::aggregate_build_options_from_path(&path, 3).unwrap();
        assert_eq!(opts.entries_sampled, 3);
        assert_eq!(opts.defines, vec!["OPT0", "OPT1", "OPT2"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_aggregate_build_options_missing_file() {
        let err = CompilationDatabase::aggregate_build_options_from_path(
            Path::new("/nonexistent/compile_commands.json"),
            10,
        );
        assert!(matches!(
            err,
            Err(CompilationDatabaseError::ReadError { .. })
        ));
    }
}
