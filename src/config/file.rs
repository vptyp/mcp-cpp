//! On-disk `.mcp-cpp.yaml` schema
//!
//! Every field is `Option`, so an absent key is distinguishable from a key set
//! to the same value as the default. Unknown keys are rejected rather than
//! ignored: a typo in a config file is otherwise invisible, and the user is
//! left believing a setting took effect when it never did.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use crate::clangd::config::PchStorage;

/// Name of the configuration file, looked up in the project root.
pub const CONFIG_FILE_NAME: &str = ".mcp-cpp.yaml";

/// Schema version this build understands.
const SUPPORTED_VERSION: u32 = 1;

/// Failure to load or understand a configuration file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("Cannot read config file {path}: {source}")]
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The file is not valid YAML, or does not match the schema.
    #[error("Invalid config file {path}: {source}")]
    Parse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying deserialization error.
        #[source]
        source: serde_norway::Error,
    },

    /// The file declares a schema version this build does not implement.
    #[error(
        "Config file {path} declares version {found}, but this build only supports version {supported}"
    )]
    UnsupportedVersion {
        /// Path of the offending file.
        path: PathBuf,
        /// Version the file declared.
        found: u32,
        /// Version this build implements.
        supported: u32,
    },
}

/// Root of the configuration file schema.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Schema version. Absent is treated as the current version.
    pub version: Option<u32>,
    /// clangd process and LSP settings.
    pub clangd: Option<ClangdSection>,
    /// Project discovery settings.
    pub project: Option<ProjectSection>,
    /// Transport settings.
    pub server: Option<ServerSection>,
}

/// `clangd:` section.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClangdSection {
    /// Path to the clangd executable.
    pub path: Option<String>,
    /// Extra arguments appended to the generated argv.
    pub args: Option<Vec<String>>,
    /// Enable clangd's background indexer.
    pub background_index: Option<bool>,
    /// Where clangd keeps preamble/PCH state.
    pub pch_storage: Option<PchStorageSetting>,
    /// Worker thread count; explicit `null` means one per CPU core.
    pub index_threads: Option<Option<u32>>,
    /// Cap on results for a workspace symbol query.
    pub workspace_symbol_limit: Option<u32>,
    /// Budget for the LSP `initialize` handshake, e.g. `30s`.
    #[serde(default, with = "humantime_serde::option")]
    pub initialization_timeout: Option<Duration>,
    /// Budget for an individual LSP request, e.g. `30s`.
    #[serde(default, with = "humantime_serde::option")]
    pub request_timeout: Option<Duration>,
    /// Default time tools wait for indexing, e.g. `20s`.
    #[serde(default, with = "humantime_serde::option")]
    pub index_wait_timeout: Option<Duration>,
}

/// Value of `clangd.pch_storage`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PchStorageSetting {
    /// Keep preambles resident in RAM.
    Memory,
    /// Spill preambles to disk.
    Disk,
}

impl From<PchStorageSetting> for PchStorage {
    fn from(value: PchStorageSetting) -> Self {
        match value {
            PchStorageSetting::Memory => PchStorage::Memory,
            PchStorageSetting::Disk => PchStorage::Disk,
        }
    }
}

/// `project:` section.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectSection {
    /// Directory depth for the initial scan.
    pub scan_depth: Option<usize>,
    /// Skip dot-directories while scanning.
    pub skip_hidden: Option<bool>,
    /// Follow symlinks while scanning.
    pub follow_symlinks: Option<bool>,
    /// Stop after this many components; explicit `null` means unlimited.
    pub max_components: Option<Option<usize>>,
}

/// `server:` section.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Bind address for the streamable-HTTP transport.
    pub host: Option<String>,
    /// Bind port for the streamable-HTTP transport.
    pub port: Option<u16>,
}

impl ConfigFile {
    /// Load `.mcp-cpp.yaml` from `dir`, if it exists.
    ///
    /// A missing file yields `Ok(None)` -- that is the normal case, not a
    /// problem. A file that exists but cannot be read or parsed is an error:
    /// starting up while silently ignoring the user's settings would be worse
    /// than refusing to start.
    pub fn load_from_dir(dir: &Path) -> Result<Option<(PathBuf, Self)>, ConfigError> {
        let path = dir.join(CONFIG_FILE_NAME);
        if !path.exists() {
            return Ok(None);
        }

        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;

        let file = Self::from_yaml(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;

        if let Some(found) = file.version
            && found != SUPPORTED_VERSION
        {
            return Err(ConfigError::UnsupportedVersion {
                path,
                found,
                supported: SUPPORTED_VERSION,
            });
        }

        Ok(Some((path, file)))
    }

    /// Parse configuration from a YAML string.
    pub fn from_yaml(text: &str) -> Result<Self, serde_norway::Error> {
        // An empty file deserializes to a null document, which is a legitimate
        // "nothing configured" rather than a schema violation.
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_norway::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_missing_file_is_not_an_error() {
        let dir = tempdir().unwrap();
        assert!(ConfigFile::load_from_dir(dir.path()).unwrap().is_none());
    }

    #[test]
    fn test_empty_file_parses_as_all_defaults() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE_NAME), "").unwrap();

        let (path, file) = ConfigFile::load_from_dir(dir.path()).unwrap().unwrap();
        assert_eq!(path, dir.path().join(CONFIG_FILE_NAME));
        assert_eq!(file, ConfigFile::default());
    }

    #[test]
    fn test_full_file_round_trips() {
        let yaml = r#"
version: 1
clangd:
  path: /usr/bin/clangd-20
  args: ["--log=verbose"]
  background_index: true
  pch_storage: disk
  index_threads: 8
  workspace_symbol_limit: 2000
  initialization_timeout: 45s
  request_timeout: 1m
  index_wait_timeout: 5s
project:
  scan_depth: 4
  skip_hidden: false
  follow_symlinks: true
  max_components: 12
server:
  host: 0.0.0.0
  port: 9000
"#;
        let file = ConfigFile::from_yaml(yaml).unwrap();
        let clangd = file.clangd.unwrap();
        assert_eq!(clangd.path.unwrap(), "/usr/bin/clangd-20");
        assert_eq!(clangd.args.unwrap(), vec!["--log=verbose"]);
        assert_eq!(clangd.pch_storage.unwrap(), PchStorageSetting::Disk);
        assert_eq!(clangd.index_threads, Some(Some(8)));
        assert_eq!(clangd.workspace_symbol_limit, Some(2000));
        assert_eq!(clangd.request_timeout, Some(Duration::from_secs(60)));

        let project = file.project.unwrap();
        assert_eq!(project.scan_depth, Some(4));
        assert_eq!(project.max_components, Some(Some(12)));

        let server = file.server.unwrap();
        assert_eq!(server.host.unwrap(), "0.0.0.0");
        assert_eq!(server.port, Some(9000));
    }

    /// A typo must be loud. Accepting unknown keys would leave the user
    /// convinced a setting is active when it is being dropped on the floor.
    #[test]
    fn test_unknown_key_is_rejected() {
        let err = ConfigFile::from_yaml("clangd:\n  pathh: /usr/bin/clangd\n").unwrap_err();
        assert!(
            err.to_string().contains("pathh"),
            "error should name the offending key, got: {err}"
        );

        assert!(ConfigFile::from_yaml("clangdd:\n  path: x\n").is_err());
    }

    #[test]
    fn test_malformed_yaml_is_an_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE_NAME), "clangd: [unclosed").unwrap();

        let err = ConfigFile::load_from_dir(dir.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn test_future_schema_version_is_rejected() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE_NAME), "version: 99\n").unwrap();

        let err = ConfigFile::load_from_dir(dir.path()).unwrap_err();
        match err {
            ConfigError::UnsupportedVersion {
                found, supported, ..
            } => {
                assert_eq!(found, 99);
                assert_eq!(supported, SUPPORTED_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn test_bad_duration_is_rejected() {
        assert!(ConfigFile::from_yaml("clangd:\n  request_timeout: soon\n").is_err());
    }
}
