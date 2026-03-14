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
use std::collections::BTreeMap;
use std::fmt::Write as _;
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

    /// `cache_size` is not a multiple of `read_size` or `write_size`,
    /// or is not a divisor of `block_size`.
    #[error(
        "cache_size ({cache_size}) must be a multiple of both read_size ({read_size}) and write_size ({write_size}), and a divisor of block_size ({block_size})"
    )]
    InvalidCacheSize {
        cache_size: usize,
        read_size: usize,
        write_size: usize,
        block_size: usize,
    },

    /// `lookahead_size` is not a multiple of 8.
    #[error("lookahead_size ({0}) must be a multiple of 8")]
    InvalidLookaheadSize(usize),

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
pub struct RawConfig {
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
    cache_size: Option<usize>,
    lookahead_size: Option<usize>,
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

        // Cache size: default to max(read_size, write_size), matching the
        // littlefs2 Rust crate's typical usage. Must be a multiple of both
        // read_size and write_size, and must evenly divide block_size.
        let cache_size = self.cache_size.unwrap_or_else(|| read_size.max(write_size));

        if cache_size % read_size != 0
            || cache_size % write_size != 0
            || self.block_size % cache_size != 0
        {
            return Err(ConfigError::InvalidCacheSize {
                cache_size,
                read_size,
                write_size,
                block_size: self.block_size,
            });
        }

        // Lookahead size in bytes: must be a multiple of 8. Default to the
        // smallest valid value that covers all blocks (rounded up to 8 bytes).
        // Minimum 8 bytes per LittleFS requirements.
        let lookahead_size = self.lookahead_size.unwrap_or_else(|| {
            let bytes_needed = (block_count + 7) / 8;
            let aligned = ((bytes_needed + 7) / 8) * 8;
            aligned.max(8)
        });

        if lookahead_size % 8 != 0 || lookahead_size == 0 {
            return Err(ConfigError::InvalidLookaheadSize(lookahead_size));
        }

        Ok(ImageConfig {
            block_size: self.block_size,
            block_count,
            read_size,
            write_size,
            block_cycles: self.block_cycles,
            cache_size,
            lookahead_size,
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
            cache_size: None,
            lookahead_size: None,
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

    /// Builder function for setting cache size
    pub fn with_cache_size(mut self, cache_size: usize) -> Self {
        self.cache_size = Some(cache_size);
        self
    }

    /// Builder function for setting lookahead size (in bytes, must be multiple of 8)
    pub fn with_lookahead_size(mut self, lookahead_size: usize) -> Self {
        self.lookahead_size = Some(lookahead_size);
        self
    }
}

/// A consistent and validated image configuration.
///
/// Construct via [`RawImageConfig::resolve`] for guaranteed validity,
/// or directly if you are managing correctness yourself.
#[derive(Clone, Debug)]
pub struct ImageConfig {
    pub block_size: usize,
    pub block_count: usize,
    pub read_size: usize,
    pub write_size: usize,
    pub block_cycles: i32,
    /// Size of the read and write caches in bytes. Must be a multiple of
    /// both `read_size` and `write_size`, and must evenly divide `block_size`.
    pub cache_size: usize,
    /// Lookahead buffer size in bytes. Must be a multiple of 8.
    /// The `littlefs2` Rust crate measures this in units of 8 bytes via
    /// the `LOOKAHEAD_SIZE` typenum, so the emitted typenum type is
    /// `lookahead_size / 8`.
    pub lookahead_size: usize,
}

impl ImageConfig {
    /// Total image size in bytes (`block_size * block_count`).
    pub fn image_size(&self) -> usize {
        self.block_size * self.block_count
    }

    /// Write resolved Rust constants to a file in `out_dir` for use with `include!()`.
    ///
    /// Generates a `littlefs_config.rs` file containing geometry constants,
    /// typenum type aliases, `TOTAL_SIZE`, and an `IMAGE` static that embeds
    /// the binary via `include_bytes!`.
    ///
    /// When `lfs_paths` is `Some((dirs, files))`, a nested `pub mod paths { … }`
    /// tree is appended that mirrors the directory layout of the packed image.
    /// Each directory becomes a Rust module with a `DIR` constant holding the
    /// LFS path, and each file becomes an `UPPER_SNAKE_CASE` `&str` constant.
    ///
    /// ```text
    /// pub mod paths {
    ///     pub mod config {
    ///         pub const DIR: &str = "/config";
    ///         pub const NETWORK_JSON: &str = "/config/network.json";
    ///     }
    /// }
    /// ```
    pub fn emit_rust(
        &self,
        out_dir: &Path,
        image_filename: &str,
        lfs_paths: Option<(&[String], &[String])>,
    ) -> Result<(), ConfigError> {
        let path = out_dir.join("littlefs_config.rs");

        // The littlefs2 Rust crate's LOOKAHEAD_SIZE is measured in units of
        // 8 bytes, so we divide by 8 for the typenum alias.
        let lookahead_typenum_units = self.lookahead_size / 8;

        let mut content = format!(
            "// Auto-generated by littlefs2-pack — do not edit.\n\
             use generic_array::typenum;\n\
             \n\
             pub const BLOCK_SIZE: usize = {};\n\
             pub const BLOCK_COUNT: usize = {};\n\
             pub const READ_SIZE: usize = {};\n\
             pub const WRITE_SIZE: usize = {};\n\
             pub const CACHE_SIZE: usize = {};\n\
             pub const LOOKAHEAD_SIZE: usize = {};\n\
             pub const TOTAL_SIZE: usize = BLOCK_SIZE * BLOCK_COUNT;\n\
             \n\
             /// Typenum alias for `littlefs2::driver::Storage::CACHE_SIZE`.\n\
             pub type CacheSize = typenum::U{};\n\
             /// Typenum alias for `littlefs2::driver::Storage::LOOKAHEAD_SIZE`.\n\
             /// Note: the littlefs2 crate measures lookahead in units of 8 bytes,\n\
             /// so this is `lookahead_size / 8`.\n\
             pub type LookaheadSize = typenum::U{};\n\
             \n\
             /// The packed LittleFS image, embedded at compile time.\n\
             pub static IMAGE: &[u8] = include_bytes!(\"{}\");\n",
            self.block_size,
            self.block_count,
            self.read_size,
            self.write_size,
            self.cache_size,
            self.lookahead_size,
            self.cache_size,
            lookahead_typenum_units,
            image_filename,
        );

        if let Some((dirs, files)) = lfs_paths {
            content.push('\n');
            emit_paths_mod(&mut content, dirs, files);
        }

        std::fs::write(&path, content).map_err(|source| ConfigError::EmitRust { path, source })
    }
}

// ---------------------------------------------------------------------------
// Path-module generation helpers
// ---------------------------------------------------------------------------

/// A tree node used to build the nested `pub mod paths { … }` structure.
#[derive(Default)]
struct PathNode {
    /// LFS path for the `DIR` constant (set for directory entries).
    dir_path: Option<String>,
    /// Files directly inside this directory: `(CONST_NAME, lfs_path)`.
    files: Vec<(String, String)>,
    /// Subdirectory modules: `mod_name → child node`.
    children: BTreeMap<String, PathNode>,
}

/// Convert a file name (e.g. `network.json`) to `UPPER_SNAKE_CASE` (`NETWORK_JSON`).
fn to_const_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else {
            // Collapse consecutive separators into a single underscore.
            if !out.ends_with('_') {
                out.push('_');
            }
        }
    }
    // Trim leading/trailing underscores.
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        return "_UNNAMED".to_string();
    }
    // Prefix with `_` if the name starts with a digit (not a valid Rust ident start).
    if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Convert a directory name (e.g. `my-config`) to a valid Rust module name (`my_config`).
fn to_mod_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        return "_unnamed".to_string();
    }
    if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{trimmed}")
    } else {
        trimmed
    }
}

/// Build a [`PathNode`] tree from sorted directory and file path slices.
fn build_path_tree(dirs: &[String], files: &[String]) -> PathNode {
    let mut root = PathNode::default();

    for dir in dirs {
        let segments = path_segments(dir);
        let node = walk_to_node(&mut root, &segments);
        node.dir_path = Some(dir.clone());
    }

    for file in files {
        let segments = path_segments(file);
        if segments.is_empty() {
            continue;
        }
        let (file_name, parent_segs) = segments.split_last().unwrap();
        let node = walk_to_node(&mut root, parent_segs);
        node.files.push((to_const_name(file_name), file.clone()));
    }

    root
}

/// Split an LFS path like `/config/network.json` into `["config", "network.json"]`.
fn path_segments(lfs_path: &str) -> Vec<&str> {
    lfs_path.split('/').filter(|s| !s.is_empty()).collect()
}

/// Walk (and lazily create) intermediate nodes to reach the node for `segments`.
fn walk_to_node<'a>(root: &'a mut PathNode, segments: &[&str]) -> &'a mut PathNode {
    let mut current = root;
    for &seg in segments {
        let mod_name = to_mod_name(seg);
        current = current.children.entry(mod_name).or_default();
    }
    current
}

/// Recursively write the `pub mod …` tree into `out`.
fn write_node(out: &mut String, node: &PathNode, indent: usize) {
    let pad = " ".repeat(indent);

    if let Some(ref dir_path) = node.dir_path {
        let _ = writeln!(out, "{pad}pub const DIR: &str = \"{dir_path}\";");
    }

    for (const_name, lfs_path) in &node.files {
        let _ = writeln!(out, "{pad}pub const {const_name}: &str = \"{lfs_path}\";");
    }

    for (mod_name, child) in &node.children {
        let _ = writeln!(out, "{pad}pub mod {mod_name} {{");
        write_node(out, child, indent + 4);
        let _ = writeln!(out, "{pad}}}");
    }
}

/// Append a `pub mod paths { … }` block to `out`.
fn emit_paths_mod(out: &mut String, dirs: &[String], files: &[String]) {
    let tree = build_path_tree(dirs, files);

    // If neither dirs nor files produced any content, skip emitting.
    if tree.children.is_empty() && tree.files.is_empty() {
        return;
    }

    out.push_str("pub mod paths {\n");

    // Root-level files (files sitting directly under `/`).
    for (const_name, lfs_path) in &tree.files {
        let _ = writeln!(out, "    pub const {const_name}: &str = \"{lfs_path}\";");
    }

    for (mod_name, child) in &tree.children {
        let _ = writeln!(out, "    pub mod {mod_name} {{");
        write_node(out, child, 8);
        let _ = writeln!(out, "    }}");
    }

    out.push_str("}\n");
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
    // Image config: cache_size
    // -------------------------------------------------------------------------

    #[test]
    fn cache_size_defaults_to_max_read_write() {
        let toml = minimal_image_toml("block_count = 64\nread_size = 16\nwrite_size = 256");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.cache_size, 256);
    }

    #[test]
    fn cache_size_explicit() {
        let toml = minimal_image_toml(
            "block_count = 64\nread_size = 16\nwrite_size = 256\ncache_size = 512",
        );
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.cache_size, 512);
    }

    #[test]
    fn cache_size_not_multiple_of_write_size_rejected() {
        let toml = minimal_image_toml(
            "block_count = 64\nread_size = 16\nwrite_size = 256\ncache_size = 300",
        );
        let err = parse_and_validate(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidCacheSize { .. }));
    }

    #[test]
    fn cache_size_not_divisor_of_block_size_rejected() {
        // 4096 % 768 != 0
        let toml = minimal_image_toml(
            "block_count = 64\nread_size = 256\nwrite_size = 256\ncache_size = 768",
        );
        let err = parse_and_validate(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidCacheSize { .. }));
    }

    // -------------------------------------------------------------------------
    // Image config: lookahead_size
    // -------------------------------------------------------------------------

    #[test]
    fn lookahead_size_defaults_to_aligned_minimum() {
        // 64 blocks → ceil(64/8) = 8, aligned to 8 → 8
        let toml = minimal_image_toml("block_count = 64\npage_size = 256");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.lookahead_size, 8);
    }

    #[test]
    fn lookahead_size_defaults_for_large_block_count() {
        // 3096 blocks → ceil(3096/8) = 387, aligned to 8 → 392
        let toml = minimal_image_toml("block_count = 3096\npage_size = 256");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.lookahead_size, 392);
        assert_eq!(config.image.lookahead_size % 8, 0);
    }

    #[test]
    fn lookahead_size_explicit() {
        let toml = minimal_image_toml("block_count = 64\npage_size = 256\nlookahead_size = 16");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.image.lookahead_size, 16);
    }

    #[test]
    fn lookahead_size_not_multiple_of_8_rejected() {
        let toml = minimal_image_toml("block_count = 64\npage_size = 256\nlookahead_size = 10");
        let err = parse_and_validate(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidLookaheadSize(_)));
    }

    #[test]
    fn lookahead_size_zero_rejected() {
        let toml = minimal_image_toml("block_count = 64\npage_size = 256\nlookahead_size = 0");
        let err = parse_and_validate(&toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidLookaheadSize(_)));
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
        // cache_size defaults to max(read_size, write_size) = 512
        assert_eq!(config.image.cache_size, 512);
        // lookahead_size defaults to ceil8(64/8) = 8
        assert_eq!(config.image.lookahead_size, 8);
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
            "block_count = 64\npage_size = 256\nread_size = 16\nwrite_size = 256",
        );
        let config = parse_and_validate(&toml).unwrap();

        let dir = tempfile::tempdir().unwrap();
        config
            .image
            .emit_rust(dir.path(), "filesystem.bin", None)
            .unwrap();

        let contents = fs::read_to_string(dir.path().join("littlefs_config.rs")).unwrap();
        assert!(contents.contains("pub const BLOCK_SIZE: usize = 4096;"));
        assert!(contents.contains("pub const BLOCK_COUNT: usize = 64;"));
        assert!(contents.contains("pub const READ_SIZE: usize = 16;"));
        assert!(contents.contains("pub const WRITE_SIZE: usize = 256;"));
        assert!(contents.contains("pub const CACHE_SIZE: usize = 256;"));
        assert!(contents.contains("pub const LOOKAHEAD_SIZE: usize = 8;"));
        assert!(contents.contains("pub const TOTAL_SIZE: usize = BLOCK_SIZE * BLOCK_COUNT;"));
        assert!(contents.contains("pub type CacheSize = typenum::U256;"));
        assert!(contents.contains("pub type LookaheadSize = typenum::U1;"));
        assert!(contents.contains(r#"include_bytes!("filesystem.bin")"#));
        // No paths module when None is passed.
        assert!(!contents.contains("pub mod paths"));
    }

    #[test]
    fn emit_rust_generates_paths_module() {
        let toml = minimal_image_toml("block_count = 64\npage_size = 256");
        let config = parse_and_validate(&toml).unwrap();

        let dirs = vec!["/config".to_string(), "/logs".to_string()];
        let files = vec![
            "/config/network.json".to_string(),
            "/index.html".to_string(),
        ];

        let dir = tempfile::tempdir().unwrap();
        config
            .image
            .emit_rust(dir.path(), "filesystem.bin", Some((&dirs, &files)))
            .unwrap();

        let contents = fs::read_to_string(dir.path().join("littlefs_config.rs")).unwrap();

        // Image constants still present.
        assert!(contents.contains("pub const BLOCK_SIZE: usize = 4096;"));

        // Paths module structure.
        assert!(contents.contains("pub mod paths {"));
        assert!(contents.contains("pub mod config {"));
        assert!(contents.contains(r#"pub const DIR: &str = "/config";"#));
        assert!(contents.contains(r#"pub const NETWORK_JSON: &str = "/config/network.json";"#));
        assert!(contents.contains("pub mod logs {"));
        assert!(contents.contains(r#"pub const DIR: &str = "/logs";"#));
        // Root-level file.
        assert!(contents.contains(r#"pub const INDEX_HTML: &str = "/index.html";"#));
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
