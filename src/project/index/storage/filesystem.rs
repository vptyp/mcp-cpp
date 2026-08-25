//! Filesystem implementation of index storage
//!
//! This module provides a concrete implementation of the IndexStorage trait
//! that reads clangd index files from the local filesystem using dependency injection.

use super::{IndexData, IndexError, IndexMetadata, IndexStorage};
use crate::clangd::index::idx_parser::{IdxParseError, IdxParser};
use crate::io::file_system::FileSystemTrait;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, info, trace};

/// How long a directory listing is reused before being refreshed.
///
/// During a rescan over a large build directory, `read_index` is called once per
/// source file and each call would otherwise re-list the index directory. Caching
/// the listing for a short window turns that O(N*M) syscall storm into a single
/// listing plus in-memory scans.
const LISTING_CACHE_TTL: Duration = Duration::from_secs(1);

/// Filesystem-based index storage implementation with dependency injection
pub struct FilesystemIndexStorage<F: FileSystemTrait> {
    /// Root directory containing index files
    index_directory: PathBuf,
    /// Index format version guessed from the clangd version string.
    ///
    /// Only a starting point: the format version moves independently of the
    /// clangd release number, so this guess is superseded by `observed_version`
    /// as soon as a real index file has been read.
    seeded_version: u32,
    /// Format version latched from the first index file we managed to parse.
    ///
    /// Zero means nothing has been observed yet. Once set it never changes, so
    /// index files that disagree with it are genuinely stale leftovers from an
    /// older clangd rather than victims of a bad guess.
    observed_version: AtomicU32,
    /// Filesystem implementation for dependency injection
    filesystem: F,
    /// Cache of the last successful directory listing (with timestamp)
    listing_cache: Mutex<Option<(Instant, Vec<PathBuf>)>>,
}

impl<F: FileSystemTrait + 'static> FilesystemIndexStorage<F> {
    /// Create a new filesystem index storage with dependency injection
    ///
    /// # Arguments
    /// * `index_directory` - Directory containing clangd index files
    /// * `seeded_version` - Index format version guessed from the clangd version
    ///   string, used only until a real index file has been parsed
    /// * `filesystem` - Filesystem implementation for testability
    pub fn new(index_directory: PathBuf, seeded_version: u32, filesystem: F) -> Self {
        Self {
            index_directory,
            seeded_version,
            observed_version: AtomicU32::new(0),
            filesystem,
            listing_cache: Mutex::new(None),
        }
    }

    /// Record the format version clangd actually wrote.
    ///
    /// The first observation wins; later files are compared against it rather
    /// than replacing it.
    fn observe_version(&self, version: u32) {
        if self
            .observed_version
            .compare_exchange(0, version, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
            && version != self.seeded_version
        {
            info!(
                "clangd index format version is {} (guessed {} from the clangd version string)",
                version, self.seeded_version
            );
        }
    }

    /// Parse an index file and extract metadata
    async fn parse_index_file(&self, index_path: &Path) -> Result<IndexData, IndexError> {
        trace!("Parsing index file: {:?}", index_path);

        // Check if file exists using filesystem trait
        let path = index_path.to_path_buf();
        let filesystem = self.filesystem.clone();

        let exists = tokio::task::spawn_blocking(move || filesystem.exists(&path))
            .await
            .map_err(|e| IndexError::Io(std::io::Error::other(e)))?;

        if !exists {
            return Err(IndexError::FileNotFound {
                path: index_path.to_path_buf(),
            });
        }

        // Read file metadata using filesystem trait
        let path = index_path.to_path_buf();
        let filesystem = self.filesystem.clone();

        let file_metadata = tokio::task::spawn_blocking(move || filesystem.metadata(&path))
            .await
            .map_err(|e| IndexError::Io(std::io::Error::other(e)))?
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::PermissionDenied {
                    IndexError::PermissionDenied {
                        path: index_path.to_path_buf(),
                    }
                } else {
                    IndexError::Io(err)
                }
            })?;

        // Read and parse the index file using the IDX parser
        let file_size = file_metadata.size;
        let created_at = Some(file_metadata.modified);

        // Read file content using filesystem trait
        let path = index_path.to_path_buf();
        let filesystem = self.filesystem.clone();

        let file_data = tokio::task::spawn_blocking(move || filesystem.read(&path))
            .await
            .map_err(|e| IndexError::Io(std::io::Error::other(e)))??;

        // Parse the index file using the IDX parser
        let parsed_data = IdxParser::parse(&file_data).map_err(|e| match e {
            IdxParseError::UnsupportedVersion(v) => {
                IndexError::incompatible_version(v, self.expected_version())
            }
            _ => IndexError::parse_error(e.to_string()),
        })?;

        // The file just told us what version clangd is really writing; that
        // beats anything derived from the clangd version string.
        self.observe_version(parsed_data.format_version);

        // Extract source file information from the include graph
        // Look for translation units first, then fall back to any file if no TUs found
        let translation_units = parsed_data.translation_units();
        let source_file = if !translation_units.is_empty() {
            // Use the first translation unit as the primary source file
            PathBuf::from(&translation_units[0].uri)
        } else if !parsed_data.include_graph.is_empty() {
            // Fall back to the first file in the include graph
            PathBuf::from(&parsed_data.include_graph[0].uri)
        } else {
            // No files found in include graph, derive from filename
            self.derive_source_path_from_index(index_path)?
        };

        // Extract content hash from the primary source file
        let content_hash =
            if let Some(node) = parsed_data.find_node_by_uri(&source_file.to_string_lossy()) {
                hex::encode(node.digest)
            } else if !translation_units.is_empty() {
                hex::encode(translation_units[0].digest)
            } else {
                "UNKNOWN_HASH".to_string()
            };

        let index_data = IndexData {
            source_file,
            format_version: parsed_data.format_version,
            content_hash,
            symbols: vec![], // Could be extracted from symb chunk in the future
            metadata: IndexMetadata {
                created_at,
                file_size: Some(file_size),
            },
        };

        debug!(
            "Parsed index file: {} bytes, format version {}, {} include graph nodes, {} TUs",
            file_size,
            index_data.format_version,
            parsed_data.include_graph.len(),
            translation_units.len()
        );

        Ok(index_data)
    }

    /// Derive source file path from index file path
    /// This is a temporary implementation - real implementation would read from index
    fn derive_source_path_from_index(&self, index_path: &Path) -> Result<PathBuf, IndexError> {
        // This is a simplified reverse mapping
        // In reality, we'd read the source file mapping from the index file itself
        let filename = index_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| IndexError::corrupted(index_path, "Invalid index filename format"))?;

        // For testing purposes, assume filename maps to a source file
        // Real implementation would maintain proper source-to-index mapping
        Ok(PathBuf::from(format!("{}.cpp", filename)))
    }
}

#[async_trait]
impl<F: FileSystemTrait + 'static> IndexStorage for FilesystemIndexStorage<F> {
    async fn read_index(&self, source_path: &Path) -> Result<IndexData, IndexError> {
        // Find the actual index file by pattern matching instead of hash computation
        let source_filename = source_path
            .file_name()
            .ok_or_else(|| IndexError::FileNotFound {
                path: source_path.to_path_buf(),
            })?
            .to_string_lossy();

        // Look for files matching the pattern: SourceFile.HASH.idx
        let pattern_prefix = format!("{}.", source_filename);

        let index_files = self
            .list_index_files(&self.index_directory)
            .await
            .unwrap_or_default();

        for index_file in index_files {
            if let Some(filename) = index_file.file_name() {
                let filename_str = filename.to_string_lossy();
                if filename_str.starts_with(&pattern_prefix) && filename_str.ends_with(".idx") {
                    trace!(
                        "Found index file for source: {:?} -> {:?}",
                        source_path, index_file
                    );
                    return self.parse_index_file(&index_file).await;
                }
            }
        }

        // No index file found for this source file
        Err(IndexError::FileNotFound {
            path: source_path.to_path_buf(),
        })
    }

    async fn list_index_files(&self, index_dir: &Path) -> Result<Vec<PathBuf>, IndexError> {
        debug!("Listing index files in: {:?}", index_dir);

        // Serve from cache if fresh (only for the storage's own index directory).
        // This avoids re-listing the directory for every source file during a rescan.
        if index_dir == self.index_directory
            && let Ok(guard) = self.listing_cache.lock()
            && let Some((ts, files)) = guard.as_ref()
            && ts.elapsed() < LISTING_CACHE_TTL
        {
            return Ok(files.clone());
        }

        // Check if directory exists using filesystem trait
        let path = index_dir.to_path_buf();
        let filesystem = self.filesystem.clone();

        let exists = tokio::task::spawn_blocking(move || filesystem.exists(&path))
            .await
            .map_err(|e| IndexError::Io(std::io::Error::other(e)))?;

        if !exists {
            return Err(IndexError::DirectoryNotFound {
                path: index_dir.to_path_buf(),
            });
        }

        // Read directory entries using filesystem trait
        let path = index_dir.to_path_buf();
        let filesystem = self.filesystem.clone();

        let entries = tokio::task::spawn_blocking(move || filesystem.read_dir(&path))
            .await
            .map_err(|e| IndexError::Io(std::io::Error::other(e)))??;

        let mut index_files = Vec::new();
        for entry_path in entries {
            // Filter for index files (typically have .idx extension in clangd)
            if let Some(extension) = entry_path.extension()
                && extension == "idx"
            {
                index_files.push(entry_path);
            }
        }

        // Cache the successful listing for the storage's own index directory
        if index_dir == self.index_directory
            && let Ok(mut guard) = self.listing_cache.lock()
        {
            *guard = Some((Instant::now(), index_files.clone()));
        }

        debug!("Found {} index files", index_files.len());
        Ok(index_files)
    }

    async fn has_index_files(&self) -> bool {
        // A single directory listing is enough to determine whether any index
        // files exist. This avoids the O(N) per-file directory listings that
        // would otherwise happen for large build directories.
        self.list_index_files(&self.index_directory)
            .await
            .map(|files| !files.is_empty())
            .unwrap_or(false)
    }

    async fn indexed_source_files(&self) -> Result<std::collections::HashSet<PathBuf>, IndexError> {
        let index_files = self.list_index_files(&self.index_directory).await?;
        let mut sources = std::collections::HashSet::new();
        for index_file in index_files {
            if let Some(name) = index_file.file_name().and_then(|s| s.to_str()) {
                // Index files are named `<source_filename>.<HASH>.idx`; strip the
                // `.idx` suffix and the trailing 16-hex-digit hash to recover the
                // source file name.
                if let Some(stripped) = name.strip_suffix(".idx")
                    && let Some(dot) = stripped.rfind('.')
                {
                    let hash = &stripped[dot + 1..];
                    if hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                        sources.insert(PathBuf::from(&stripped[..dot]));
                    }
                }
            }
        }
        Ok(sources)
    }

    fn supports_version(&self, version: u32) -> bool {
        // Support current version and one version back for compatibility
        let expected = self.expected_version();
        version == expected || version == expected.saturating_sub(1)
    }

    fn expected_version(&self) -> u32 {
        match self.observed_version.load(Ordering::Relaxed) {
            0 => self.seeded_version,
            observed => observed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::file_system::TestFileSystem;
    use tempfile::TempDir;

    #[test]
    fn test_filesystem_storage_creation() {
        let temp_dir = TempDir::new().unwrap();
        let filesystem = TestFileSystem::new();
        let storage = FilesystemIndexStorage::new(temp_dir.path().to_path_buf(), 19, filesystem);

        assert_eq!(storage.expected_version(), 19);
        assert!(storage.supports_version(19));
        assert!(storage.supports_version(18)); // One back
        assert!(!storage.supports_version(17)); // Too old
        assert!(!storage.supports_version(20)); // Too new
    }

    #[test]
    fn test_observed_version_supersedes_seeded_guess() {
        let temp_dir = TempDir::new().unwrap();
        let filesystem = TestFileSystem::new();
        let storage = FilesystemIndexStorage::new(temp_dir.path().to_path_buf(), 19, filesystem);

        // The clangd-version-string guess stands only until a file contradicts it.
        assert_eq!(storage.expected_version(), 19);

        storage.observe_version(20);
        assert_eq!(storage.expected_version(), 20);
        assert!(storage.supports_version(20));
        assert!(
            !storage.supports_version(18),
            "two versions back is genuinely stale"
        );

        // First observation wins, so one stale leftover cannot redefine what the
        // workspace considers current.
        storage.observe_version(17);
        assert_eq!(storage.expected_version(), 20);
    }

    #[tokio::test]
    async fn test_read_nonexistent_index() {
        let temp_dir = TempDir::new().unwrap();
        let filesystem = TestFileSystem::new();
        let storage = FilesystemIndexStorage::new(temp_dir.path().to_path_buf(), 19, filesystem);

        let source_path = Path::new("/project/src/main.cpp");
        let result = storage.read_index(source_path).await;

        assert!(matches!(result, Err(IndexError::FileNotFound { .. })));
    }

    #[tokio::test]
    async fn test_list_index_files_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let filesystem = TestFileSystem::new();
        // Add directory to test filesystem (directories are tracked via file paths)
        filesystem.set_file_content(
            temp_dir.path().join(".keep"),
            "",
            std::time::SystemTime::now(),
        );
        let storage = FilesystemIndexStorage::new(temp_dir.path().to_path_buf(), 19, filesystem);

        let files = storage.list_index_files(temp_dir.path()).await.unwrap();
        assert!(files.is_empty());
    }

    #[tokio::test]
    async fn test_list_index_files_nonexistent_directory() {
        let temp_dir = TempDir::new().unwrap();
        let nonexistent = temp_dir.path().join("nonexistent");
        let filesystem = TestFileSystem::new();
        let storage = FilesystemIndexStorage::new(temp_dir.path().to_path_buf(), 19, filesystem);

        let result = storage.list_index_files(&nonexistent).await;
        assert!(matches!(result, Err(IndexError::DirectoryNotFound { .. })));
    }

    #[tokio::test]
    async fn test_has_index_files_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let filesystem = TestFileSystem::new();
        // Add directory to test filesystem (directories are tracked via file paths)
        filesystem.set_file_content(
            temp_dir.path().join(".keep"),
            "",
            std::time::SystemTime::now(),
        );
        let storage = FilesystemIndexStorage::new(temp_dir.path().to_path_buf(), 19, filesystem);

        assert!(!storage.has_index_files().await);
    }

    #[tokio::test]
    async fn test_has_index_files_nonexistent_directory() {
        let temp_dir = TempDir::new().unwrap();
        let filesystem = TestFileSystem::new();
        let storage = FilesystemIndexStorage::new(
            temp_dir.path().join("nonexistent").to_path_buf(),
            19,
            filesystem,
        );

        assert!(!storage.has_index_files().await);
    }

    #[tokio::test]
    async fn test_list_index_files_cached() {
        let temp_dir = TempDir::new().unwrap();
        let filesystem = TestFileSystem::new();
        filesystem.set_file_content(
            temp_dir.path().join("main.cpp.ABC.idx"),
            "",
            std::time::SystemTime::now(),
        );
        let storage = FilesystemIndexStorage::new(temp_dir.path().to_path_buf(), 19, filesystem);

        // Repeated listings within the TTL should return consistent results
        let first = storage.list_index_files(temp_dir.path()).await.unwrap();
        let second = storage.list_index_files(temp_dir.path()).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
    }
}
