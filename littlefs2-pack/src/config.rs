//! # littlefs2-config
//!
//! Shared configuration for LittleFS image creation and firmware integration.
//!
//! This crate parses a `littlefs.toml` file that defines the parameters of a
//! LittleFS filesystem image. The same configuration is used by:
//!
//! - **`littlefs2-tool`** to build the image from a local directory
//! - **`littlefs2-pack`** to set up LittleFS parameters for the C library
//! - **Firmware `build.rs`** to generate compile-time constants via [`ImageConfig::emit_rust`]
//!
//! # Example TOML
//!
//! ```toml
//! [image]
//! block_size = 4096
//! page_size = 256
//! block_count = 128
//!
//! [directory]
//! root = "./website"
//! depth = -1
//! ignore_hidden = true
//! gitignore = true
//! repo_gitignore = true
//! glob_ignores = ["*.bkup", "build"]
//! glob_includes = []
//! ```

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can occur when loading or validating a configuration file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The configuration file could not be read from disk.
    #[error("failed to read {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The TOML content could not be parsed or mapped to the config structs.
    #[error("failed to parse config")]
    Parse(#[from] toml::de::Error),

    /// Both `block_count` and `image_size` were specified. Only one is allowed.
    #[error("specify block_count or image_size, not both")]
    BothSizingMethods,

    /// Neither `block_count` nor `image_size` was specified. One is required.
    #[error("specify either block_count or image_size")]
    NoSizingMethod,

    /// `image_size` is not an exact multiple of `block_size`.
    #[error("image_size ({image_size}) must be a multiple of block_size ({block_size})")]
    ImageSizeAlignment {
        image_size: usize,
        block_size: usize,
    },

    /// A required size field is missing and `page_size` was not set as a fallback.
    #[error("{0} is required when page_size is not set")]
    MissingSize(&'static str),

    /// The configured root directory does not exist on disk.
    #[error("root directory not found at: {0}")]
    RootNotFound(PathBuf),

    /// A glob pattern in `glob_ignores` or `glob_includes` is invalid.
    #[error("invalid glob pattern \"{pattern}\": {source}")]
    InvalidGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },

    /// The `depth` value is invalid (must be >= -1).
    #[error("invalid depth: {0} (must be >= -1)")]
    InvalidDepth(i32),

    /// `repo_gitignore` is `true` but `gitignore` is `false`.
    #[error("repo_gitignore requires gitignore to be enabled")]
    RepoGitignoreWithoutGitignore,

    /// Failed to write generated Rust constants.
    #[error("failed to write generated config to {path}")]
    EmitRust {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Raw top-level configuration as deserialized from a TOML file.
///
/// This is an intermediate representation — use [`Config::from_file`] to
/// obtain a fully validated and resolved [`Config`].
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    image: RawImageConfig,
    directory: RawDirectoryConfig,
}

/// A fully validated and resolved configuration.
///
/// All fields have been checked for consistency and the directory root
/// has been resolved to an absolute path. Obtain via [`Config::from_file`].
#[derive(Debug)]
pub struct Config {
    pub image: ImageConfig,
    pub directory: DirectoryConfig,
    base_dir: PathBuf,
}

/// Returns the default block cycle count: -1 (no wear leveling).
fn default_block_cycles() -> i32 {
    -1
}

impl Config {
    /// Load, validate, and resolve a configuration from a TOML file.
    ///
    /// Parses the file into raw config types, then resolves both the
    /// image and directory sections into their validated forms.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        let raw: RawConfig = toml::from_str(&contents)?;
        let base_dir = path.parent().unwrap_or(Path::new(".")).to_owned();

        let image = raw.image.resolve()?;
        let directory = raw.directory.resolve(&base_dir)?;

        Ok(Config {
            image,
            directory,
            base_dir,
        })
    }

    /// The parent directory of the TOML file, used to resolve relative paths.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
}

/// LittleFS image parameters.
///
/// Defines the geometry and sizing of the filesystem image. Supports two
/// mutually exclusive ways to specify the total size: `block_count` or
/// `image_size`. The `page_size` field acts as a default for `read_size`
/// and `write_size` when they are not explicitly set.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawImageConfig {
    block_size: usize,
    block_count: Option<usize>,
    image_size: Option<usize>,
    page_size: Option<usize>,
    read_size: Option<usize>,
    write_size: Option<usize>,
    #[serde(default = "default_block_cycles")]
    block_cycles: i32,
}

impl RawImageConfig {
    /// Validate and resolve into a checked [`ImageConfig`].
    ///
    /// All field validation, default resolution (e.g. `page_size` fallback),
    /// and computed fields (e.g. `block_count` from `image_size`) are handled
    /// here. The resulting `ImageConfig` contains only concrete values.
    pub fn resolve(self) -> Result<ImageConfig, ConfigError> {
        let read_size = self
            .read_size
            .or(self.page_size)
            .ok_or(ConfigError::MissingSize("read_size"))?;

        let write_size = self
            .write_size
            .or(self.page_size)
            .ok_or(ConfigError::MissingSize("write_size"))?;

        let block_count = match (self.block_count, self.image_size) {
            (Some(c), None) => c,
            (None, Some(s)) if s % self.block_size == 0 => s / self.block_size,
            (None, Some(s)) => {
                return Err(ConfigError::ImageSizeAlignment {
                    image_size: s,
                    block_size: self.block_size,
                });
            }
            (Some(_), Some(_)) => return Err(ConfigError::BothSizingMethods),
            (None, None) => return Err(ConfigError::NoSizingMethod),
        };

        Ok(ImageConfig {
            block_size: self.block_size,
            block_count,
            read_size,
            write_size,
            block_cycles: self.block_cycles,
        })
    }

    /// Create a new unconfigured `RawImageConfig` to use with builder methods.
    ///
    /// The builder starts in an invalid state — fields must be set via
    /// the `with_*` methods before calling [`resolve`](Self::resolve)
    /// to obtain a validated [`ImageConfig`].
    pub fn new() -> Self {
        Self {
            block_size: 16,
            block_count: None,
            image_size: None,
            page_size: None,
            read_size: None,
            write_size: None,
            block_cycles: -1,
        }
    }

    /// Builder function for setting block size
    pub fn with_block_size(mut self, block_size: usize) -> Self {
        self.block_size = block_size;
        self
    }

    /// Builder function for setting block count
    pub fn with_block_count(mut self, block_count: usize) -> Self {
        self.block_count = Some(block_count);
        self
    }

    /// Builder function for setting image size
    pub fn with_image_size(mut self, image_size: usize) -> Self {
        self.image_size = Some(image_size);
        self
    }

    /// Builder function for setting page size
    pub fn with_page_size(mut self, page_size: usize) -> Self {
        self.page_size = Some(page_size);
        self
    }

    /// Builder function for setting read size
    pub fn with_read_size(mut self, read_size: usize) -> Self {
        self.read_size = Some(read_size);
        self
    }

    /// Builder function for setting write size
    pub fn with_write_size(mut self, write_size: usize) -> Self {
        self.write_size = Some(write_size);
        self
    }

    /// Builder function for setting block cycles
    pub fn with_block_cycles(mut self, block_cycles: i32) -> Self {
        self.block_cycles = block_cycles;
        self
    }
}

/// A consistent and validated image configuration.
///
/// Construct via [`RawImageConfig::resolve`] for guaranteed validity,
/// or directly if you are managing correctness yourself.
#[derive(Debug)]
pub struct ImageConfig {
    pub block_size: usize,
    pub block_count: usize,
    pub read_size: usize,
    pub write_size: usize,
    pub block_cycles: i32,
}

impl ImageConfig {
    /// Total image size in bytes (`block_size * block_count`).
    pub fn image_size(&self) -> usize {
        self.block_size * self.block_count
    }

    /// Write resolved Rust constants to a file in `out_dir` for use with `include!()`.
    ///
    /// Generates a `littlefs_config.rs` file containing `BLOCK_SIZE`, `BLOCK_COUNT`,
    /// `READ_SIZE`, and `WRITE_SIZE` as `usize` constants.
    pub fn emit_rust(&self, out_dir: &Path) -> Result<(), ConfigError> {
        let path = out_dir.join("littlefs_config.rs");
        std::fs::write(
            &path,
            format!(
                "pub const BLOCK_SIZE: usize = {};\n\
                 pub const BLOCK_COUNT: usize = {};\n\
                 pub const READ_SIZE: usize = {};\n\
                 pub const WRITE_SIZE: usize = {};\n",
                self.block_size, self.block_count, self.read_size, self.write_size,
            ),
        )
        .map_err(|source| ConfigError::EmitRust { path, source })
    }
}

/// Directory traversal settings for collecting files into the image.
///
/// Controls which local directory to pack, how deep to recurse, and
/// which files to include or exclude via gitignore rules and glob patterns.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDirectoryConfig {
    root: String,
    depth: i32,
    ignore_hidden: bool,
    gitignore: bool,
    repo_gitignore: bool,
    glob_ignores: Vec<String>,
    glob_includes: Vec<String>,
}

impl RawDirectoryConfig {
    /// Validate and resolve into a checked [`DirectoryConfig`].
    ///
    /// All field validation (depth, gitignore coherence, glob patterns),
    /// root path resolution, and glob set compilation are handled here.
    /// The resulting `DirectoryConfig` is ready to use for packing.
    pub fn resolve(self, base: &Path) -> Result<DirectoryConfig, ConfigError> {
        if self.depth < -1 {
            return Err(ConfigError::InvalidDepth(self.depth));
        }

        if self.repo_gitignore && !self.gitignore {
            return Err(ConfigError::RepoGitignoreWithoutGitignore);
        }

        for pattern in &self.glob_ignores {
            Glob::new(pattern).map_err(|source| ConfigError::InvalidGlob {
                pattern: pattern.clone(),
                source,
            })?;
        }

        // Validate and build the include GlobSet in one pass
        let include_set = if self.glob_includes.is_empty() {
            None
        } else {
            let mut builder = GlobSetBuilder::new();
            for pattern in &self.glob_includes {
                let glob = Glob::new(pattern).map_err(|source| ConfigError::InvalidGlob {
                    pattern: pattern.clone(),
                    source,
                })?;
                builder.add(glob);
            }
            Some(builder.build().expect("individual globs already validated"))
        };

        let resolved_root = base.join(&self.root);
        if !resolved_root.is_dir() {
            return Err(ConfigError::RootNotFound(resolved_root));
        }

        Ok(DirectoryConfig {
            resolved_root,
            depth: self.depth,
            ignore_hidden: self.ignore_hidden,
            gitignore: self.gitignore,
            repo_gitignore: self.repo_gitignore,
            glob_ignores: self.glob_ignores,
            glob_includes: self.glob_includes,
            include_set,
        })
    }
}

/// A validated and resolved directory configuration, ready for packing.
///
/// Construct via [`RawDirectoryConfig::resolve`] for guaranteed validity,
/// or directly if you are managing correctness yourself.
#[derive(Debug)]
pub struct DirectoryConfig {
    /// The fully resolved root directory path.
    pub resolved_root: PathBuf,
    /// Maximum recursive directory depth. -1 means unlimited.
    pub depth: i32,
    /// Whether to skip hidden files and directories.
    pub ignore_hidden: bool,
    /// Whether to respect `.gitignore` files in the directory tree.
    pub gitignore: bool,
    /// Whether to also respect the repository-level `.gitignore`.
    pub repo_gitignore: bool,
    /// Glob patterns for files and directories to exclude.
    pub glob_ignores: Vec<String>,
    /// Glob patterns for files to force-include, superseding all ignore rules.
    pub glob_includes: Vec<String>,
    /// Compiled glob set for force-included files, or `None` if no
    /// include patterns were specified. Built from `glob_includes`.
    pub include_set: Option<GlobSet>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// Parse a TOML string, validate, and resolve into a Config.
    /// Skips file I/O; directory resolves against ".".
    fn parse_and_validate(toml: &str) -> Result<Config, ConfigError> {
        let raw: RawConfig = toml::from_str(toml).map_err(ConfigError::Parse)?;
        let base_dir = PathBuf::from(".");
        let image = raw.image.resolve()?;
        let directory = raw.directory.resolve(&base_dir)?;
        Ok(Config {
            image,
            directory,
            base_dir,
        })
    }

    fn minimal_image_toml(image_section: &str) -> String {
        format!(
            r#"
[image]
block_size = 4096
{image_section}

[directory]
root = "."
depth = -1
ignore_hidden = true
gitignore = false
repo_gitignore = false
glob_ignores = []
glob_includes = []
"#
        )
    }

    // -------------------------------------------------------------------------
    // Image config: sizing (block_count vs image_size)
    // -------------------------------------------------------------------------

    #[test]
    fn block_count_directly() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.block_count, 128);
        assert_eq!(config.image.image_size(), 128 * 4096);
    }

    #[test]
    fn image_size_calculates_block_count() {
        let toml = minimal_image_toml("image_size = 524288\npage_size = 256");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.block_count, 128);
        assert_eq!(config.image.image_size(), 524288);
    }

    #[test]
    fn both_sizing_methods_rejected() {
        let toml = minimal_image_toml("block_count = 128\nimage_size = 524288\npage_size = 256");
        let err = parse_and_validate(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::BothSizingMethods));
    }

    #[test]
    fn no_sizing_method_rejected() {
        let toml = minimal_image_toml("page_size = 256");
        let err = parse_and_validate(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::NoSizingMethod));
    }

    #[test]
    fn image_size_not_multiple_of_block_size() {
        let toml = minimal_image_toml("image_size = 5000\npage_size = 256");
        let err = parse_and_validate(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::ImageSizeAlignment { .. }));
    }

    // -------------------------------------------------------------------------
    // Image config: page_size / read_size / write_size fallback
    // -------------------------------------------------------------------------

    #[test]
    fn page_size_sets_both_read_and_write() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.read_size, 256);
        assert_eq!(config.image.write_size, 256);
    }

    #[test]
    fn explicit_read_write_override_page_size() {
        let toml = minimal_image_toml(
            "block_count = 128\npage_size = 256\nread_size = 16\nwrite_size = 512",
        );
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.read_size, 16);
        assert_eq!(config.image.write_size, 512);
    }

    #[test]
    fn partial_override_with_page_size_fallback() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256\nread_size = 16");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.read_size, 16);
        assert_eq!(config.image.write_size, 256);
    }

    #[test]
    fn explicit_read_write_without_page_size() {
        let toml = minimal_image_toml("block_count = 128\nread_size = 16\nwrite_size = 512");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.read_size, 16);
        assert_eq!(config.image.write_size, 512);
    }

    #[test]
    fn missing_read_size_without_page_size() {
        let toml = minimal_image_toml("block_count = 128\nwrite_size = 512");
        let err = parse_and_validate(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::MissingSize("read_size")));
    }

    #[test]
    fn missing_write_size_without_page_size() {
        let toml = minimal_image_toml("block_count = 128\nread_size = 16");
        let err = parse_and_validate(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::MissingSize("write_size")));
    }

    // -------------------------------------------------------------------------
    // Image config: block_cycles default
    // -------------------------------------------------------------------------

    #[test]
    fn block_cycles_defaults_to_negative_one() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.block_cycles, -1);
    }

    #[test]
    fn block_cycles_explicit() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256\nblock_cycles = 500");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.block_cycles, 500);
    }

    // -------------------------------------------------------------------------
    // Image config: block_size resolved
    // -------------------------------------------------------------------------

    #[test]
    fn block_size_resolved() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.block_size, 4096);
    }

    // -------------------------------------------------------------------------
    // Directory config: resolved fields
    // -------------------------------------------------------------------------

    #[test]
    fn directory_resolved_fields() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256");
        let config = parse_and_validate(&toml).unwrap();

        assert!(config.directory.resolved_root.ends_with("."));
        assert_eq!(config.directory.depth, -1);
        assert!(config.directory.ignore_hidden);
        assert!(!config.directory.gitignore);
        assert!(!config.directory.repo_gitignore);
        assert!(config.directory.glob_ignores.is_empty());
        assert!(config.directory.include_set.is_none());
    }

    #[test]
    fn directory_with_globs() {
        let toml = r#"
[image]
block_size = 4096
block_count = 128
page_size = 256

[directory]
root = "."
depth = 3
ignore_hidden = false
gitignore = true
repo_gitignore = true
glob_ignores = ["*.bkup", "build"]
glob_includes = ["important.txt"]
"#;
        let config = parse_and_validate(toml).unwrap();

        assert_eq!(config.directory.depth, 3);
        assert!(!config.directory.ignore_hidden);
        assert!(config.directory.gitignore);
        assert!(config.directory.repo_gitignore);
        assert_eq!(config.directory.glob_ignores, &["*.bkup", "build"]);
        assert!(config.directory.include_set.is_some());
    }

    // -------------------------------------------------------------------------
    // Directory config: validation
    // -------------------------------------------------------------------------

    #[test]
    fn invalid_depth_rejected() {
        let toml = r#"
[image]
block_size = 4096
block_count = 128
page_size = 256

[directory]
root = "."
depth = -2
ignore_hidden = true
gitignore = false
repo_gitignore = false
glob_ignores = []
glob_includes = []
"#;
        let err = parse_and_validate(toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidDepth(-2)));
    }

    #[test]
    fn repo_gitignore_without_gitignore_rejected() {
        let toml = r#"
[image]
block_size = 4096
block_count = 128
page_size = 256

[directory]
root = "."
depth = -1
ignore_hidden = true
gitignore = false
repo_gitignore = true
glob_ignores = []
glob_includes = []
"#;
        let err = parse_and_validate(toml).unwrap_err();
        assert!(matches!(err, ConfigError::RepoGitignoreWithoutGitignore));
    }

    #[test]
    fn invalid_glob_ignore_rejected() {
        let toml = r#"
[image]
block_size = 4096
block_count = 128
page_size = 256

[directory]
root = "."
depth = -1
ignore_hidden = true
gitignore = false
repo_gitignore = false
glob_ignores = ["[invalid"]
glob_includes = []
"#;
        let err = parse_and_validate(toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidGlob { .. }));
    }

    #[test]
    fn invalid_glob_include_rejected() {
        let toml = r#"
[image]
block_size = 4096
block_count = 128
page_size = 256

[directory]
root = "."
depth = -1
ignore_hidden = true
gitignore = false
repo_gitignore = false
glob_ignores = []
glob_includes = ["[also-invalid"]
"#;
        let err = parse_and_validate(toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidGlob { .. }));
    }

    #[test]
    fn valid_globs_accepted() {
        let toml = r#"
[image]
block_size = 4096
block_count = 128
page_size = 256

[directory]
root = "."
depth = -1
ignore_hidden = true
gitignore = false
repo_gitignore = false
glob_ignores = ["*.bkup", "build", "**/*.tmp"]
glob_includes = ["important.txt", "data/**"]
"#;
        assert!(parse_and_validate(toml).is_ok());
    }

    // -------------------------------------------------------------------------
    // Directory config: resolution
    // -------------------------------------------------------------------------

    #[test]
    fn resolve_root_existing_directory() {
        let dir_config = RawDirectoryConfig {
            root: ".".to_string(),
            depth: -1,
            ignore_hidden: true,
            gitignore: false,
            repo_gitignore: false,
            glob_ignores: vec![],
            glob_includes: vec![],
        };
        let result = dir_config.resolve(Path::new("."));
        assert!(result.is_ok());
    }

    #[test]
    fn resolve_root_missing_directory() {
        let dir_config = RawDirectoryConfig {
            root: "nonexistent_dir_that_should_not_exist".to_string(),
            depth: -1,
            ignore_hidden: true,
            gitignore: false,
            repo_gitignore: false,
            glob_ignores: vec![],
            glob_includes: vec![],
        };
        let err = dir_config.resolve(Path::new(".")).unwrap_err();
        assert!(matches!(err, ConfigError::RootNotFound(_)));
    }

    // -------------------------------------------------------------------------
    // Full round-trip: from_file
    // -------------------------------------------------------------------------

    #[test]
    fn from_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let website_dir = dir.path().join("website");
        fs::create_dir(&website_dir).unwrap();

        let toml_path = dir.path().join("littlefs.toml");
        fs::write(
            &toml_path,
            r#"
[image]
block_size = 4096
block_count = 64
page_size = 256
read_size = 16
write_size = 512
block_cycles = 100

[directory]
root = "./website"
depth = -1
ignore_hidden = true
gitignore = true
repo_gitignore = false
glob_ignores = ["*.tmp"]
glob_includes = []
"#,
        )
        .unwrap();

        let config = Config::from_file(&toml_path).unwrap();

        assert_eq!(config.base_dir(), dir.path());
        assert_eq!(config.image.block_size, 4096);
        assert_eq!(config.image.block_count, 64);
        assert_eq!(config.image.image_size(), 64 * 4096);
        assert_eq!(config.image.read_size, 16);
        assert_eq!(config.image.write_size, 512);
        assert_eq!(config.image.block_cycles, 100);
        assert_eq!(config.directory.resolved_root, dir.path().join("website"));
        assert!(config.directory.ignore_hidden);
    }

    #[test]
    fn from_file_missing_file() {
        let err = Config::from_file(Path::new("does_not_exist.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn from_file_missing_root_directory() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("littlefs.toml");
        fs::write(
            &toml_path,
            r#"
[image]
block_size = 4096
block_count = 128
page_size = 256

[directory]
root = "./missing"
depth = -1
ignore_hidden = true
gitignore = false
repo_gitignore = false
glob_ignores = []
glob_includes = []
"#,
        )
        .unwrap();

        let err = Config::from_file(&toml_path).unwrap_err();
        assert!(matches!(err, ConfigError::RootNotFound(_)));
    }

    // -------------------------------------------------------------------------
    // emit_rust
    // -------------------------------------------------------------------------

    #[test]
    fn emit_rust_generates_constants() {
        let toml = minimal_image_toml(
            "block_count = 64\npage_size = 256\nread_size = 16\nwrite_size = 512",
        );
        let config = parse_and_validate(&toml).unwrap();

        let dir = tempfile::tempdir().unwrap();
        config.image.emit_rust(dir.path()).unwrap();

        let contents = fs::read_to_string(dir.path().join("littlefs_config.rs")).unwrap();
        assert!(contents.contains("pub const BLOCK_SIZE: usize = 4096;"));
        assert!(contents.contains("pub const BLOCK_COUNT: usize = 64;"));
        assert!(contents.contains("pub const READ_SIZE: usize = 16;"));
        assert!(contents.contains("pub const WRITE_SIZE: usize = 512;"));
    }

    // -------------------------------------------------------------------------
    // TOML parsing: unknown fields rejected
    // -------------------------------------------------------------------------

    #[test]
    fn unknown_image_field_rejected() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256\nbogus = 42");
        let err = parse_and_validate(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn unknown_directory_field_rejected() {
        let toml = r#"
[image]
block_size = 4096
block_count = 128
page_size = 256

[directory]
root = "."
depth = -1
ignore_hidden = true
gitignore = false
repo_gitignore = false
glob_ignores = []
glob_includes = []
surprise = true
"#;
        let err = parse_and_validate(toml).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }
}
