use std::path::{Path, PathBuf};

use ignore::{WalkBuilder, overrides::OverrideBuilder};
use littlefs2_config::DirectoryConfig;
use littlefs2_pack::MountedFs;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PackError {
    #[error("LittleFS error: {0}")]
    Lfs(#[from] littlefs2_pack::LfsError),

    #[error("directory walk error: {0}")]
    Walk(#[from] ignore::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("path is not valid UTF-8: {}", .0.display())]
    InvalidPath(PathBuf),
}

/// Build a `WalkBuilder` from a `DirectoryConfig`.
///
/// Applie the depth, hidden-file, gitignore, and glob settings
/// from the TOML configuration.
pub fn walker(config: &DirectoryConfig, root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder.hidden(config.ignore_hidden());

    let depth = config.depth();
    if depth >= 0 {
        builder.max_depth(Some(depth as usize));
    }

    builder
        .git_ignore(config.gitignore())
        .git_global(config.repo_gitignore());

    let mut overrides = OverrideBuilder::new(root);

    // Negate patterns to ignore them
    for pattern in config.glob_ignores() {
        overrides
            .add(&format!("!{pattern}"))
            .expect("glob patterns are validated when DirectoryConfig is created");
    }

    // Include patterns override ignores — added after so they win
    for pattern in config.glob_includes() {
        overrides
            .add(pattern)
            .expect("glob patterns are validated when DirectoryConfig is created");
    }

    builder.overrides(
        overrides
            .build()
            .expect("glob patterns are validated when DirectoryConfig is created"),
    );

    builder
}

/// Convert a host path to a LittleFS path by stripping the root prefix.
///
/// `./website/css/style.css` with root `./website` becomes `/css/style.css`.
fn to_lfs_path(host_path: &Path, root: &Path) -> Result<String, PackError> {
    let relative = host_path
        .strip_prefix(root)
        .map_err(|_| PackError::InvalidPath(host_path.to_owned()))?;

    let s = relative
        .to_str()
        .ok_or_else(|| PackError::InvalidPath(host_path.to_owned()))?;

    Ok(format!("/{s}"))
}

/// Walk a directory and pack its contents into a mounted LittleFS filesystem.
///
/// The caller is responsible for creating, formatting, and mounting
/// the image. This function just writes the directory contents into it.
pub fn pack_directory(
    fs: &MountedFs<'_>,
    config: &DirectoryConfig,
    root: &Path,
) -> Result<(), PackError> {
    let walk = walker(config, root);

    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();

    for entry in walk.build() {
        let entry = entry?;

        // Skip the root directory itself
        if entry.depth() == 0 {
            continue;
        }

        let ft = entry
            .file_type()
            .ok_or_else(|| PackError::InvalidPath(entry.path().to_owned()))?;

        let lfs_path = to_lfs_path(entry.path(), root)?;

        if ft.is_dir() {
            dirs.push(lfs_path);
        } else if ft.is_file() {
            let data = std::fs::read(entry.path())?;
            files.push((lfs_path, data));
        }
    }

    // Sort for deterministic output
    dirs.sort();
    files.sort_by(|(a, _), (b, _)| a.cmp(b));

    for path in &dirs {
        fs.create_dir_all(path)?;
    }
    for (path, data) in &files {
        fs.write_file(path, data)?;
    }

    Ok(())
}

/// Simple recursive directory packing without ignore/glob rules.
/// Used when no TOML config is provided.
pub fn pack_directory_simple(
    fs: &MountedFs<'_>,
    host_dir: &Path,
    lfs_prefix: &str,
) -> Result<(), PackError> {
    let mut entries: Vec<_> = std::fs::read_dir(host_dir)
        .map_err(|e| PackError::Io(e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| PackError::Io(e))?;

    // Sort for deterministic output
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let file_type = entry.file_type().map_err(|e| PackError::Io(e))?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let lfs_path = if lfs_prefix.is_empty() {
            format!("/{name_str}")
        } else {
            format!("{lfs_prefix}/{name_str}")
        };

        if file_type.is_dir() {
            println!("  mkdir  {lfs_path}");
            fs.create_dir(&lfs_path)?;
            pack_directory_simple(fs, &entry.path(), &lfs_path)?;
        } else if file_type.is_file() {
            let data = std::fs::read(entry.path()).map_err(|e| PackError::Io(e))?;
            println!("  write  {lfs_path} ({} bytes)", data.len());
            fs.write_file(&lfs_path, &data)?;
        }
    }

    Ok(())
}
