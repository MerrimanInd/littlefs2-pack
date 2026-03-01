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

use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// By convention, all struct fields are private with accessor
/// functions. This macro reduces boilerplate for fields where
/// the return type matches the field type (Copy types).
macro_rules! accessor {
    ($name:ident -> $ty:ty) => {
        pub fn $name(&self) -> $ty {
            self.$name
        }
    };
}

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

    /// Failed to write generated Rust constants.
    #[error("failed to write generated config to {path}")]
    EmitRust {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Top-level configuration loaded from a `littlefs.toml` file.
///
/// Contains both the image parameters and directory settings, along with
/// the base directory (parent of the TOML file) used to resolve relative paths.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub image: ImageConfig,
    pub directory: DirectoryConfig,

    /// The parent directory of the TOML file, used to resolve relative paths.
    /// Not part of the TOML schema — populated after deserialization.
    #[serde(skip)]
    base_dir: PathBuf,
}

/// Returns the default block cycle count: -1 (no wear leveling).
fn default_block_cycles() -> i32 {
    -1
}

impl Config {
    /// Load and validate a configuration from a TOML file.
    ///
    /// This parses the file, validates image parameters and directory
    /// settings, and resolves the root directory relative to the TOML
    /// file's location.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        let mut config: Config = toml::from_str(&contents)?;
        config.base_dir = path.parent().unwrap_or(Path::new(".")).to_owned();

        config.image.validate()?;
        config.directory.resolve_root(&config.base_dir)?;

        Ok(config)
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
pub struct ImageConfig {
    block_size: usize,
    block_count: Option<usize>,
    image_size: Option<usize>,
    page_size: Option<usize>,
    read_size: Option<usize>,
    write_size: Option<usize>,
    #[serde(default = "default_block_cycles")]
    block_cycles: i32,
}

impl ImageConfig {
    // The filesystem block (erase unit) size in bytes.
    accessor!(block_size -> usize);

    // Block-cycle count for wear leveling. -1 disables wear leveling.
    accessor!(block_cycles -> i32);

    // Create a new ImageConfig object, mainly for testing purposes
    pub fn new(block_size: usize, block_count: usize, read_size: usize, write_size: usize) -> Self {
        Self {
            block_size,
            block_count: Some(block_count),
            image_size: None,
            page_size: None,
            read_size: Some(read_size),
            write_size: Some(write_size),
            block_cycles: -1,
        }
    }

    /// Validate that the image configuration is internally consistent.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.read_size.is_none() && self.page_size.is_none() {
            return Err(ConfigError::MissingSize("read_size"));
        }

        if self.write_size.is_none() && self.page_size.is_none() {
            return Err(ConfigError::MissingSize("write_size"));
        }

        match (self.block_count, self.image_size) {
            (Some(_), Some(_)) => Err(ConfigError::BothSizingMethods),
            (None, None) => Err(ConfigError::NoSizingMethod),
            (None, Some(s)) if s % self.block_size != 0 => Err(ConfigError::ImageSizeAlignment {
                image_size: s,
                block_size: self.block_size,
            }),
            _ => Ok(()),
        }
    }

    /// Return the block count, either directly or calculated from `image_size / block_size`.
    pub fn block_count(&self) -> usize {
        match (self.block_count, self.image_size) {
            (Some(c), None) => c,
            (None, Some(s)) => s / self.block_size,
            _ => unreachable!("Config::validate() ensures exactly one is set"),
        }
    }

    /// Return the image size in bytes, either directly or calculated from `block_count * block_size`.
    pub fn image_size(&self) -> usize {
        match (self.block_count, self.image_size) {
            (None, Some(s)) => s,
            (Some(c), None) => c * self.block_size,
            _ => unreachable!("Config::validate() ensures exactly one is set"),
        }
    }

    /// Return the read size, falling back to `page_size` if not explicitly set.
    pub fn read_size(&self) -> usize {
        self.read_size.or(self.page_size).expect("validated")
    }

    /// Return the write (program) size, falling back to `page_size` if not explicitly set.
    pub fn write_size(&self) -> usize {
        self.write_size.or(self.page_size).expect("validated")
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
                self.block_size(),
                self.block_count(),
                self.read_size(),
                self.write_size(),
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
pub struct DirectoryConfig {
    root: String,
    depth: i32,
    ignore_hidden: bool,
    gitignore: bool,
    repo_gitignore: bool,
    glob_ignores: Vec<String>,
    glob_includes: Vec<String>,
}

impl DirectoryConfig {
    /// Maximum recursive directory depth. -1 means unlimited.
    accessor!(depth -> i32);

    /// Whether to skip hidden files and directories.
    accessor!(ignore_hidden -> bool);

    /// Whether to respect `.gitignore` files in the directory tree.
    accessor!(gitignore -> bool);

    /// Whether to also respect the repository-level `.gitignore`.
    /// Only meaningful when `gitignore` is `true`.
    accessor!(repo_gitignore -> bool);

    /// The configured root directory path (as written in the TOML).
    pub fn root(&self) -> &str {
        &self.root
    }

    /// Glob patterns for files and directories to exclude.
    pub fn glob_ignores(&self) -> &[String] {
        &self.glob_ignores
    }

    /// Glob patterns for files to force-include, superseding all ignore rules.
    pub fn glob_includes(&self) -> &[String] {
        &self.glob_includes
    }

    /// Resolve the root path against a base directory and verify it exists.
    pub fn resolve_root(&self, base: &Path) -> Result<PathBuf, ConfigError> {
        let root = base.join(&self.root);
        if !root.is_dir() {
            return Err(ConfigError::RootNotFound(root));
        }
        Ok(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// Parse a TOML string directly into a Config, skipping file I/O and
    /// directory resolution. Runs image validation only.
    fn parse_and_validate_image(toml: &str) -> Result<Config, ConfigError> {
        let mut config: Config = toml::from_str(toml).map_err(ConfigError::Parse)?;
        config.base_dir = PathBuf::from(".");
        config.image.validate()?;
        Ok(config)
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
        let config = parse_and_validate_image(&toml).unwrap();
        assert_eq!(config.image.block_count(), 128);
        assert_eq!(config.image.image_size(), 128 * 4096);
    }

    #[test]
    fn image_size_calculates_block_count() {
        let toml = minimal_image_toml("image_size = 524288\npage_size = 256");
        let config = parse_and_validate_image(&toml).unwrap();
        assert_eq!(config.image.block_count(), 128);
        assert_eq!(config.image.image_size(), 524288);
    }

    #[test]
    fn both_sizing_methods_rejected() {
        let toml = minimal_image_toml("block_count = 128\nimage_size = 524288\npage_size = 256");
        let err = parse_and_validate_image(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::BothSizingMethods));
    }

    #[test]
    fn no_sizing_method_rejected() {
        let toml = minimal_image_toml("page_size = 256");
        let err = parse_and_validate_image(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::NoSizingMethod));
    }

    #[test]
    fn image_size_not_multiple_of_block_size() {
        let toml = minimal_image_toml("image_size = 5000\npage_size = 256");
        let err = parse_and_validate_image(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::ImageSizeAlignment { .. }));
    }

    // -------------------------------------------------------------------------
    // Image config: page_size / read_size / write_size fallback
    // -------------------------------------------------------------------------

    #[test]
    fn page_size_sets_both_read_and_write() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256");
        let config = parse_and_validate_image(&toml).unwrap();
        assert_eq!(config.image.read_size(), 256);
        assert_eq!(config.image.write_size(), 256);
    }

    #[test]
    fn explicit_read_write_override_page_size() {
        let toml = minimal_image_toml(
            "block_count = 128\npage_size = 256\nread_size = 16\nwrite_size = 512",
        );
        let config = parse_and_validate_image(&toml).unwrap();
        assert_eq!(config.image.read_size(), 16);
        assert_eq!(config.image.write_size(), 512);
    }

    #[test]
    fn partial_override_with_page_size_fallback() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256\nread_size = 16");
        let config = parse_and_validate_image(&toml).unwrap();
        assert_eq!(config.image.read_size(), 16);
        assert_eq!(config.image.write_size(), 256);
    }

    #[test]
    fn explicit_read_write_without_page_size() {
        let toml = minimal_image_toml("block_count = 128\nread_size = 16\nwrite_size = 512");
        let config = parse_and_validate_image(&toml).unwrap();
        assert_eq!(config.image.read_size(), 16);
        assert_eq!(config.image.write_size(), 512);
    }

    #[test]
    fn missing_read_size_without_page_size() {
        let toml = minimal_image_toml("block_count = 128\nwrite_size = 512");
        let err = parse_and_validate_image(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::MissingSize("read_size")));
    }

    #[test]
    fn missing_write_size_without_page_size() {
        let toml = minimal_image_toml("block_count = 128\nread_size = 16");
        let err = parse_and_validate_image(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::MissingSize("write_size")));
    }

    // -------------------------------------------------------------------------
    // Image config: block_cycles default
    // -------------------------------------------------------------------------

    #[test]
    fn block_cycles_defaults_to_negative_one() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256");
        let config = parse_and_validate_image(&toml).unwrap();
        assert_eq!(config.image.block_cycles(), -1);
    }

    #[test]
    fn block_cycles_explicit() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256\nblock_cycles = 500");
        let config = parse_and_validate_image(&toml).unwrap();
        assert_eq!(config.image.block_cycles(), 500);
    }

    // -------------------------------------------------------------------------
    // Image config: block_size accessor
    // -------------------------------------------------------------------------

    #[test]
    fn block_size_accessor() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256");
        let config = parse_and_validate_image(&toml).unwrap();
        assert_eq!(config.image.block_size(), 4096);
    }

    // -------------------------------------------------------------------------
    // Directory config: accessors
    // -------------------------------------------------------------------------

    #[test]
    fn directory_accessors() {
        let toml = minimal_image_toml("block_count = 128\npage_size = 256");
        let config = parse_and_validate_image(&toml).unwrap();
        let dir = &config.directory;

        assert_eq!(dir.root(), ".");
        assert_eq!(dir.depth(), -1);
        assert!(dir.ignore_hidden());
        assert!(!dir.gitignore());
        assert!(!dir.repo_gitignore());
        assert!(dir.glob_ignores().is_empty());
        assert!(dir.glob_includes().is_empty());
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
        let config = parse_and_validate_image(toml).unwrap();
        let dir = &config.directory;

        assert_eq!(dir.depth(), 3);
        assert!(!dir.ignore_hidden());
        assert!(dir.gitignore());
        assert!(dir.repo_gitignore());
        assert_eq!(dir.glob_ignores(), &["*.bkup", "build"]);
        assert_eq!(dir.glob_includes(), &["important.txt"]);
    }

    // -------------------------------------------------------------------------
    // Directory config: root resolution
    // -------------------------------------------------------------------------

    #[test]
    fn resolve_root_existing_directory() {
        let dir_config = DirectoryConfig {
            root: ".".to_string(),
            depth: -1,
            ignore_hidden: true,
            gitignore: false,
            repo_gitignore: false,
            glob_ignores: vec![],
            glob_includes: vec![],
        };
        let result = dir_config.resolve_root(Path::new("."));
        assert!(result.is_ok());
    }

    #[test]
    fn resolve_root_missing_directory() {
        let dir_config = DirectoryConfig {
            root: "nonexistent_dir_that_should_not_exist".to_string(),
            depth: -1,
            ignore_hidden: true,
            gitignore: false,
            repo_gitignore: false,
            glob_ignores: vec![],
            glob_includes: vec![],
        };
        let err = dir_config.resolve_root(Path::new(".")).unwrap_err();
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
        assert_eq!(config.image.block_size(), 4096);
        assert_eq!(config.image.block_count(), 64);
        assert_eq!(config.image.image_size(), 64 * 4096);
        assert_eq!(config.image.read_size(), 16);
        assert_eq!(config.image.write_size(), 512);
        assert_eq!(config.image.block_cycles(), 100);
        assert_eq!(config.directory.root(), "./website");
        assert!(config.directory.ignore_hidden());
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
        let config = parse_and_validate_image(&toml).unwrap();

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
        let err = parse_and_validate_image(&toml).unwrap_err();
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
        let err = parse_and_validate_image(toml).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }
}
