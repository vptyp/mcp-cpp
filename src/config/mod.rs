//! Layered application configuration
//!
//! Resolves settings from four sources, highest priority first:
//!
//! 1. CLI arguments
//! 2. environment variables
//! 3. `.mcp-cpp.yaml` in the project root
//! 4. compiled-in defaults
//!
//! The point of the file layer is portability: the same checked-out repository
//! behaves the same on every machine that shares it, and per-machine details
//! (a distro-specific clangd path, a smaller thread count on a laptop) move out
//! of source code and into a file the user owns.
//!
//! Every field is optional in the file. A missing file is not an error -- it is
//! the overwhelmingly common case and simply leaves the defaults in place. A
//! *malformed* file is an error, because silently ignoring a config the user
//! wrote is worse than refusing to start.

mod file;

pub use file::ConfigError;
use file::ConfigFile;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::clangd::config::{
    DEFAULT_INDEX_WAIT_TIMEOUT_SECS, DEFAULT_INITIALIZATION_TIMEOUT_SECS,
    DEFAULT_REQUEST_TIMEOUT_SECS, DEFAULT_WORKSPACE_SYMBOL_LIMIT, PchStorage,
};
use crate::project::ScanOptions;

/// Default depth for the initial project scan.
///
/// Deep enough to find `build/`, `out/release/` and similar nested layouts,
/// shallow enough not to walk an entire monorepo on startup.
pub const DEFAULT_SCAN_DEPTH: usize = 3;

/// Default host for the streamable-HTTP transport.
///
/// Loopback on purpose: this server exposes a project's full source tree, so
/// binding it to a routable address must be a deliberate act, not a default.
pub const DEFAULT_HTTP_HOST: &str = "127.0.0.1";

/// Default port for the streamable-HTTP transport.
pub const DEFAULT_HTTP_PORT: u16 = 8080;

/// Fully resolved configuration, with every source already merged.
#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    /// Project root that was scanned, and that the config file was loaded from.
    pub project_root: PathBuf,
    /// Path of the config file actually loaded, if any.
    pub source_file: Option<PathBuf>,
    /// clangd process and LSP settings.
    pub clangd: ClangdSettings,
    /// Project discovery settings.
    pub project: ProjectSettings,
    /// Transport settings.
    pub server: ServerSettings,
}

/// Resolved clangd settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ClangdSettings {
    /// Path to the clangd executable.
    pub path: String,
    /// Extra arguments appended to the generated argv.
    pub extra_args: Vec<String>,
    /// Enable clangd's background indexer.
    pub background_index: bool,
    /// Where clangd keeps preamble/PCH state.
    pub pch_storage: PchStorage,
    /// Indexer/worker thread count; `None` means "one per CPU core".
    pub index_threads: Option<u32>,
    /// Cap on results returned for a workspace symbol query.
    pub workspace_symbol_limit: u32,
    /// Budget for the LSP `initialize` handshake.
    pub initialization_timeout: Duration,
    /// Budget for an individual LSP request.
    pub request_timeout: Duration,
    /// Default time tools wait for indexing before answering anyway.
    pub index_wait_timeout: Duration,
}

/// Resolved project discovery settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSettings {
    /// Directory depth for the initial scan.
    pub scan_depth: usize,
    /// Skip dot-directories while scanning.
    pub skip_hidden: bool,
    /// Follow symlinks while scanning.
    pub follow_symlinks: bool,
    /// Stop after this many components; `None` means unlimited.
    pub max_components: Option<usize>,
}

/// Resolved transport settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSettings {
    /// Bind address for the streamable-HTTP transport.
    pub host: String,
    /// Bind port for the streamable-HTTP transport.
    pub port: u16,
}

/// Overrides supplied on the command line.
///
/// A `None` field means "not specified", which is materially different from a
/// specified value that happens to equal the default -- only the former defers
/// to the environment and the config file.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--clangd-path`
    pub clangd_path: Option<String>,
    /// `--host`
    pub host: Option<String>,
    /// `--port`
    pub port: Option<u16>,
}

impl Default for ClangdSettings {
    fn default() -> Self {
        Self {
            path: "clangd".to_string(),
            extra_args: Vec::new(),
            background_index: true,
            pch_storage: PchStorage::Memory,
            index_threads: None,
            workspace_symbol_limit: DEFAULT_WORKSPACE_SYMBOL_LIMIT,
            initialization_timeout: Duration::from_secs(DEFAULT_INITIALIZATION_TIMEOUT_SECS),
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
            index_wait_timeout: Duration::from_secs(DEFAULT_INDEX_WAIT_TIMEOUT_SECS),
        }
    }
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            scan_depth: DEFAULT_SCAN_DEPTH,
            skip_hidden: true,
            follow_symlinks: false,
            max_components: None,
        }
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            host: DEFAULT_HTTP_HOST.to_string(),
            port: DEFAULT_HTTP_PORT,
        }
    }
}

impl AppConfig {
    /// Resolve configuration for `project_root`, merging all four sources.
    ///
    /// Reads `.mcp-cpp.yaml` from `project_root` if present. Returns an error
    /// only when a file exists but cannot be read or parsed.
    pub fn resolve(
        project_root: &Path,
        cli: &CliOverrides,
        env: &dyn EnvSource,
    ) -> Result<Self, ConfigError> {
        let loaded = ConfigFile::load_from_dir(project_root)?;
        Ok(Self::merge(project_root, cli, env, loaded))
    }

    /// Merge already-loaded sources. Split out from [`resolve`](Self::resolve)
    /// so precedence can be tested without touching the filesystem.
    fn merge(
        project_root: &Path,
        cli: &CliOverrides,
        env: &dyn EnvSource,
        loaded: Option<(PathBuf, ConfigFile)>,
    ) -> Self {
        let (source_file, file) = match loaded {
            Some((path, file)) => (Some(path), file),
            None => (None, ConfigFile::default()),
        };

        let mut clangd = ClangdSettings::default();
        let mut project = ProjectSettings::default();
        let mut server = ServerSettings::default();

        // --- file layer -----------------------------------------------------
        if let Some(c) = &file.clangd {
            if let Some(v) = &c.path {
                clangd.path = v.clone();
            }
            if let Some(v) = &c.args {
                clangd.extra_args = v.clone();
            }
            if let Some(v) = c.background_index {
                clangd.background_index = v;
            }
            if let Some(v) = c.pch_storage {
                clangd.pch_storage = v.into();
            }
            // `index_threads` is itself nullable, so an explicit `null` in the
            // file means "one per core" rather than "unset".
            if let Some(v) = c.index_threads {
                clangd.index_threads = v;
            }
            if let Some(v) = c.workspace_symbol_limit {
                clangd.workspace_symbol_limit = v;
            }
            if let Some(v) = c.initialization_timeout {
                clangd.initialization_timeout = v;
            }
            if let Some(v) = c.request_timeout {
                clangd.request_timeout = v;
            }
            if let Some(v) = c.index_wait_timeout {
                clangd.index_wait_timeout = v;
            }
        }
        if let Some(p) = &file.project {
            if let Some(v) = p.scan_depth {
                project.scan_depth = v;
            }
            if let Some(v) = p.skip_hidden {
                project.skip_hidden = v;
            }
            if let Some(v) = p.follow_symlinks {
                project.follow_symlinks = v;
            }
            if let Some(v) = p.max_components {
                project.max_components = v;
            }
        }
        if let Some(s) = &file.server {
            if let Some(v) = &s.host {
                server.host = v.clone();
            }
            if let Some(v) = s.port {
                server.port = v;
            }
        }

        // --- environment layer ----------------------------------------------
        if let Some(v) = env.get("CLANGD_PATH") {
            clangd.path = v;
        }

        // --- CLI layer -------------------------------------------------------
        if let Some(v) = &cli.clangd_path {
            clangd.path = v.clone();
        }
        if let Some(v) = &cli.host {
            server.host = v.clone();
        }
        if let Some(v) = cli.port {
            server.port = v;
        }

        Self {
            project_root: project_root.to_path_buf(),
            source_file,
            clangd,
            project,
            server,
        }
    }

    /// Scanner options implied by the resolved project settings.
    pub fn scan_options(&self) -> ScanOptions {
        ScanOptions {
            skip_hidden: self.project.skip_hidden,
            follow_symlinks: self.project.follow_symlinks,
            max_components: self.project.max_components,
        }
    }
}

impl AppConfig {
    /// Minimal configuration for tests: defaults everywhere except the clangd path.
    #[cfg(test)]
    #[allow(dead_code)] // only used by the clangd-integration-tests suites
    pub fn for_test(project_root: &Path, clangd_path: String) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            source_file: None,
            clangd: ClangdSettings {
                path: clangd_path,
                ..Default::default()
            },
            project: ProjectSettings::default(),
            server: ServerSettings::default(),
        }
    }
}

/// Source of environment variables.
///
/// A trait rather than a direct `std::env::var` call so precedence tests do not
/// have to mutate real process environment, which is global and racy under a
/// multi-threaded test runner.
pub trait EnvSource {
    /// Look up a variable, returning `None` when unset or non-UTF-8.
    fn get(&self, key: &str) -> Option<String>;
}

/// Reads the real process environment.
pub struct SystemEnv;

impl EnvSource for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapEnv(HashMap<String, String>);

    impl MapEnv {
        fn empty() -> Self {
            Self(HashMap::new())
        }
        fn with(key: &str, value: &str) -> Self {
            let mut m = HashMap::new();
            m.insert(key.to_string(), value.to_string());
            Self(m)
        }
    }

    impl EnvSource for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn parse(yaml: &str) -> Option<(PathBuf, ConfigFile)> {
        Some((
            PathBuf::from("/proj/.mcp-cpp.yaml"),
            ConfigFile::from_yaml(yaml).unwrap(),
        ))
    }

    #[test]
    fn test_defaults_when_nothing_is_configured() {
        let cfg = AppConfig::merge(
            Path::new("/proj"),
            &CliOverrides::default(),
            &MapEnv::empty(),
            None,
        );

        assert_eq!(cfg.clangd.path, "clangd");
        assert_eq!(
            cfg.clangd.workspace_symbol_limit,
            DEFAULT_WORKSPACE_SYMBOL_LIMIT
        );
        assert_eq!(cfg.project.scan_depth, DEFAULT_SCAN_DEPTH);
        assert_eq!(cfg.server.host, DEFAULT_HTTP_HOST);
        assert_eq!(cfg.server.port, DEFAULT_HTTP_PORT);
        assert_eq!(cfg.source_file, None);
    }

    /// The whole point of the layering: each source must beat the one below it.
    #[test]
    fn test_precedence_cli_beats_env_beats_file_beats_default() {
        let yaml = "clangd:\n  path: /from/file/clangd\n";

        // file beats default
        let cfg = AppConfig::merge(
            Path::new("/proj"),
            &CliOverrides::default(),
            &MapEnv::empty(),
            parse(yaml),
        );
        assert_eq!(cfg.clangd.path, "/from/file/clangd");

        // env beats file
        let cfg = AppConfig::merge(
            Path::new("/proj"),
            &CliOverrides::default(),
            &MapEnv::with("CLANGD_PATH", "/from/env/clangd"),
            parse(yaml),
        );
        assert_eq!(cfg.clangd.path, "/from/env/clangd");

        // cli beats env
        let cli = CliOverrides {
            clangd_path: Some("/from/cli/clangd".to_string()),
            ..Default::default()
        };
        let cfg = AppConfig::merge(
            Path::new("/proj"),
            &cli,
            &MapEnv::with("CLANGD_PATH", "/from/env/clangd"),
            parse(yaml),
        );
        assert_eq!(cfg.clangd.path, "/from/cli/clangd");
    }

    /// An unset field in the file must not clobber the default with a zero value.
    #[test]
    fn test_partial_file_leaves_other_fields_at_defaults() {
        let cfg = AppConfig::merge(
            Path::new("/proj"),
            &CliOverrides::default(),
            &MapEnv::empty(),
            parse("project:\n  scan_depth: 7\n"),
        );

        assert_eq!(cfg.project.scan_depth, 7);
        assert!(cfg.project.skip_hidden);
        assert!(!cfg.project.follow_symlinks);
        assert_eq!(cfg.clangd.path, "clangd");
        assert_eq!(cfg.server.port, DEFAULT_HTTP_PORT);
    }

    #[test]
    fn test_durations_parse_from_human_readable_strings() {
        let cfg = AppConfig::merge(
            Path::new("/proj"),
            &CliOverrides::default(),
            &MapEnv::empty(),
            parse(
                "clangd:\n  request_timeout: 90s\n  initialization_timeout: 2m\n  index_wait_timeout: 500ms\n",
            ),
        );

        assert_eq!(cfg.clangd.request_timeout, Duration::from_secs(90));
        assert_eq!(cfg.clangd.initialization_timeout, Duration::from_secs(120));
        assert_eq!(cfg.clangd.index_wait_timeout, Duration::from_millis(500));
    }

    /// `index_threads` is doubly optional: absent means "leave the default",
    /// explicit null means "one thread per core". Both land on `None` here but
    /// only the explicit form is a decision the user made.
    #[test]
    fn test_index_threads_null_means_one_per_core() {
        let cfg = AppConfig::merge(
            Path::new("/proj"),
            &CliOverrides::default(),
            &MapEnv::empty(),
            parse("clangd:\n  index_threads: null\n"),
        );
        assert_eq!(cfg.clangd.index_threads, None);

        let cfg = AppConfig::merge(
            Path::new("/proj"),
            &CliOverrides::default(),
            &MapEnv::empty(),
            parse("clangd:\n  index_threads: 4\n"),
        );
        assert_eq!(cfg.clangd.index_threads, Some(4));
    }

    #[test]
    fn test_scan_options_follow_project_settings() {
        let cfg = AppConfig::merge(
            Path::new("/proj"),
            &CliOverrides::default(),
            &MapEnv::empty(),
            parse("project:\n  skip_hidden: false\n  follow_symlinks: true\n  max_components: 5\n"),
        );

        let opts = cfg.scan_options();
        assert!(!opts.skip_hidden);
        assert!(opts.follow_symlinks);
        assert_eq!(opts.max_components, Some(5));
    }
}
