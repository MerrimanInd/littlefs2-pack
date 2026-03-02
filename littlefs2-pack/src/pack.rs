use std::path::{Path, PathBuf};

use crate::config::DirectoryConfig;
use crate::littlefs::MountedFs;
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PackError {
    #[error("LittleFS error: {0}")]
    Lfs(#[from] crate::littlefs::LfsError),

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
pub(crate) fn walker(config: &DirectoryConfig, root: &Path) -> WalkBuilder {
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
///
/// `glob_includes` patterns are handled via a separate rescue walk: a
/// second pass with all ignore rules disabled that picks up any files
/// matching an include pattern that the main walk skipped.
pub fn pack_directory(
    fs: &MountedFs<'_>,
    config: &DirectoryConfig,
    root: &Path,
) -> Result<(), PackError> {
    let walk = walker(config, root);

    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Main walk: go through the directory and collect all of the files and
    // directories except for those matching the negative ignore configs.
    for entry in walk.build() {
        let entry = entry?;

        // The first entry in a walk is always the root top-level directory
        if entry.depth() == 0 {
            continue;
        }

        let ft = entry
            .file_type()
            .ok_or_else(|| PackError::InvalidPath(entry.path().to_owned()))?;

        let lfs_path = to_lfs_path(entry.path(), root)?;

        if ft.is_dir() {
            seen.insert(lfs_path.clone());
            dirs.push(lfs_path);
        } else if ft.is_file() {
            seen.insert(lfs_path.clone());
            let data = std::fs::read(entry.path())?;
            files.push((lfs_path, data));
        }
    }

    // Rescue walk: pick up files matching glob_includes that the main
    // walk skipped (because of hidden-file rules, gitignore, or glob_ignores).
    if let Some(include_set) = config.include_set() {
        let mut rescue = WalkBuilder::new(root);
        rescue
            .hidden(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);

        let depth = config.depth();
        if depth >= 0 {
            rescue.max_depth(Some(depth as usize));
        }

        for entry in rescue.build() {
            let entry = entry?;

            if entry.depth() == 0 {
                continue;
            }

            let ft = entry
                .file_type()
                .ok_or_else(|| PackError::InvalidPath(entry.path().to_owned()))?;

            let lfs_path = to_lfs_path(entry.path(), root)?;

            // Already picked up by the main walk
            if seen.contains(&lfs_path) {
                continue;
            }

            // Only rescue files/dirs that match an include pattern.
            // Match against the file/dir name, not the full path.
            let name = entry
                .path()
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();

            if !include_set.is_match(name.as_ref()) {
                continue;
            }

            if ft.is_dir() {
                seen.insert(lfs_path.clone());
                dirs.push(lfs_path);
            } else if ft.is_file() {
                // Ensure parent directories of rescued files are created.
                // The parent might have been skipped by the main walk
                // (e.g. a hidden directory containing a rescued file).
                if let Some(parent) = entry.path().parent() {
                    if parent != root {
                        let parent_lfs = to_lfs_path(parent, root)?;
                        if !seen.contains(&parent_lfs) {
                            seen.insert(parent_lfs.clone());
                            dirs.push(parent_lfs);
                        }
                    }
                }
                seen.insert(lfs_path.clone());
                let data = std::fs::read(entry.path())?;
                files.push((lfs_path, data));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DirectoryConfig, ImageConfig};
    use crate::littlefs::{LfsError, LfsImage};
    use std::fs;

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    fn test_image_config() -> ImageConfig {
        ImageConfig::from(4096, 16, 256, 256)
    }

    /// Wrap pack functions for use inside `mount_and_then` (which requires `LfsError`).
    fn pack_err(r: Result<(), PackError>) -> Result<(), LfsError> {
        r.map_err(|e| LfsError::Io(e.to_string()))
    }

    /// Build a DirectoryConfig by templating a full TOML string.
    /// Each parameter has a default that can be overridden by name.
    fn make_dir_config(
        depth: i32,
        ignore_hidden: bool,
        glob_ignores: &[&str],
        glob_includes: &[&str],
    ) -> DirectoryConfig {
        let ignores = glob_ignores
            .iter()
            .map(|g| format!("\"{g}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let includes = glob_includes
            .iter()
            .map(|g| format!("\"{g}\""))
            .collect::<Vec<_>>()
            .join(", ");

        let toml = format!(
            r#"
[image]
block_size = 4096
block_count = 16
page_size = 256

[directory]
root = "."
depth = {depth}
ignore_hidden = {ignore_hidden}
gitignore = false
repo_gitignore = false
glob_ignores = [{ignores}]
glob_includes = [{includes}]
"#
        );
        let config: Config = toml::from_str(&toml).unwrap();
        config.directory
    }

    fn default_dir_config() -> DirectoryConfig {
        make_dir_config(-1, true, &[], &[])
    }

    /// Create a temp directory with a known file structure:
    ///
    ///   root/
    ///     index.html        "<html>hello</html>"
    ///     .hidden           "secret"
    ///     css/
    ///       style.css       "body {}"
    ///     js/
    ///       app.js          "console.log('hi')"
    ///     build/
    ///       output.bin      "binary data"
    fn create_test_directory(root: &Path) {
        fs::create_dir_all(root.join("css")).unwrap();
        fs::create_dir_all(root.join("js")).unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("index.html"), "<html>hello</html>").unwrap();
        fs::write(root.join("css/style.css"), "body {}").unwrap();
        fs::write(root.join("js/app.js"), "console.log('hi')").unwrap();
        fs::write(root.join(".hidden"), "secret").unwrap();
        fs::write(root.join("build/output.bin"), "binary data").unwrap();
    }

    /// Collect all file names from a walker (skipping the root entry).
    fn walk_file_names(walk: WalkBuilder) -> Vec<String> {
        walk.build()
            .filter_map(|e| e.ok())
            .filter(|e| e.depth() > 0)
            .filter(|e| e.file_type().map_or(false, |ft| ft.is_file()))
            .map(|e| e.path().file_name().unwrap().to_string_lossy().to_string())
            .collect()
    }

    // -------------------------------------------------------------------------
    // to_lfs_path
    // -------------------------------------------------------------------------

    #[test]
    fn lfs_path_strips_root() {
        let result =
            to_lfs_path(Path::new("./website/css/style.css"), Path::new("./website")).unwrap();
        assert_eq!(result, "/css/style.css");
    }

    #[test]
    fn lfs_path_file_at_root() {
        let result =
            to_lfs_path(Path::new("/tmp/site/index.html"), Path::new("/tmp/site")).unwrap();
        assert_eq!(result, "/index.html");
    }

    #[test]
    fn lfs_path_deeply_nested() {
        let result = to_lfs_path(Path::new("site/a/b/c/d.txt"), Path::new("site")).unwrap();
        assert_eq!(result, "/a/b/c/d.txt");
    }

    #[test]
    fn lfs_path_wrong_root_fails() {
        let err =
            to_lfs_path(Path::new("/tmp/site/index.html"), Path::new("/tmp/other")).unwrap_err();
        assert!(matches!(err, PackError::InvalidPath(_)));
    }

    // -------------------------------------------------------------------------
    // walker: hidden files
    // -------------------------------------------------------------------------

    #[test]
    fn walker_hides_hidden_files() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = default_dir_config();
        let files = walk_file_names(walker(&config, dir.path()));

        assert!(!files.contains(&".hidden".to_string()));
        assert!(files.contains(&"index.html".to_string()));
    }

    #[test]
    fn walker_shows_hidden_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = make_dir_config(-1, false, &[], &[]);
        let files = walk_file_names(walker(&config, dir.path()));

        assert!(files.contains(&".hidden".to_string()));
        assert!(files.contains(&"index.html".to_string()));
    }

    // -------------------------------------------------------------------------
    // walker: depth limiting
    // -------------------------------------------------------------------------

    #[test]
    fn walker_respects_depth_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("top.txt"), "top").unwrap();
        fs::write(root.join("a/mid.txt"), "mid").unwrap();
        fs::write(root.join("a/b/deep.txt"), "deep").unwrap();
        fs::write(root.join("a/b/c/too_deep.txt"), "too deep").unwrap();

        let config = make_dir_config(2, true, &[], &[]);
        let files = walk_file_names(walker(&config, root));

        assert!(files.contains(&"top.txt".to_string()));
        assert!(files.contains(&"mid.txt".to_string()));
        assert!(!files.contains(&"too_deep.txt".to_string()));
    }

    #[test]
    fn walker_unlimited_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b/c/d")).unwrap();
        fs::write(root.join("a/b/c/d/deep.txt"), "deep").unwrap();

        let config = default_dir_config();
        let files = walk_file_names(walker(&config, root));

        assert!(files.contains(&"deep.txt".to_string()));
    }

    // -------------------------------------------------------------------------
    // walker: glob ignores
    // -------------------------------------------------------------------------

    #[test]
    fn walker_glob_ignores_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = make_dir_config(-1, true, &["*.bin"], &[]);
        let files = walk_file_names(walker(&config, dir.path()));

        assert!(!files.contains(&"output.bin".to_string()));
        assert!(files.contains(&"index.html".to_string()));
    }

    #[test]
    fn walker_glob_ignores_directory() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = make_dir_config(-1, true, &["build"], &[]);
        let all_paths: Vec<PathBuf> = walker(&config, dir.path())
            .build()
            .filter_map(|e| e.ok())
            .filter(|e| e.depth() > 0)
            .map(|e| e.path().to_owned())
            .collect();

        let has_build = all_paths
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == "build"));
        assert!(!has_build, "build directory should be excluded");
    }

    // -------------------------------------------------------------------------
    // walker: glob includes override ignores
    // -------------------------------------------------------------------------

    #[test]
    fn walker_glob_includes_override_ignores() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("keep.bin"), "keep").unwrap();
        fs::write(root.join("drop.bin"), "drop").unwrap();
        fs::write(root.join("also.txt"), "also").unwrap();

        // When a positive override ("keep.bin") is present, the ignore crate
        // treats it as a whitelist: only files matching a positive pattern are
        // included. So "also.txt" is excluded too — it doesn't match "keep.bin".
        let config = make_dir_config(-1, false, &["*.bin"], &["keep.bin"]);
        let files = walk_file_names(walker(&config, root));

        assert!(files.contains(&"keep.bin".to_string()));
        assert!(!files.contains(&"drop.bin".to_string()));
        assert!(!files.contains(&"also.txt".to_string()));
    }

    // -------------------------------------------------------------------------
    // pack_directory: integration with LfsImage
    // -------------------------------------------------------------------------

    #[test]
    fn pack_creates_correct_structure() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let dir_config = default_dir_config();
        let mut image = LfsImage::new(test_image_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                pack_err(pack_directory(fs, &dir_config, dir.path()))?;

                assert!(fs.exists("/index.html"));
                assert!(fs.exists("/css/style.css"));
                assert!(fs.exists("/js/app.js"));

                let html = fs.read_file("/index.html")?;
                assert_eq!(html, b"<html>hello</html>");

                let css = fs.read_file("/css/style.css")?;
                assert_eq!(css, b"body {}");

                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn pack_respects_hidden_ignore() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let dir_config = default_dir_config();
        let mut image = LfsImage::new(test_image_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                pack_err(pack_directory(fs, &dir_config, dir.path()))?;
                assert!(!fs.exists("/.hidden"));
                assert!(fs.exists("/index.html"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn pack_includes_hidden_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let dir_config = make_dir_config(-1, false, &[], &[]);
        let mut image = LfsImage::new(test_image_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                pack_err(pack_directory(fs, &dir_config, dir.path()))?;
                assert!(fs.exists("/.hidden"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn pack_with_glob_ignores() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let dir_config = make_dir_config(-1, true, &["build"], &[]);
        let mut image = LfsImage::new(test_image_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                pack_err(pack_directory(fs, &dir_config, dir.path()))?;
                assert!(!fs.exists("/build"));
                assert!(!fs.exists("/build/output.bin"));
                assert!(fs.exists("/index.html"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn pack_empty_directory() {
        let dir = tempfile::tempdir().unwrap();

        let dir_config = default_dir_config();
        let mut image = LfsImage::new(test_image_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                pack_err(pack_directory(fs, &dir_config, dir.path()))?;
                let entries = fs.read_dir("/")?;
                assert!(entries.is_empty());
                Ok(())
            })
            .unwrap();
    }

    // -------------------------------------------------------------------------
    // pack_directory: deterministic output
    // -------------------------------------------------------------------------

    #[test]
    fn pack_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let dir_config = default_dir_config();

        let pack_once = || {
            let mut image = LfsImage::new(test_image_config()).unwrap();
            image.format().unwrap();
            image
                .mount_and_then(|fs| pack_err(pack_directory(fs, &dir_config, dir.path())))
                .unwrap();
            image.into_data()
        };

        assert_eq!(pack_once(), pack_once());
    }

    // -------------------------------------------------------------------------
    // pack_directory_simple
    // -------------------------------------------------------------------------

    #[test]
    fn simple_pack_includes_everything() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let mut image = LfsImage::new(test_image_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                pack_err(pack_directory_simple(fs, dir.path(), ""))?;
                assert!(fs.exists("/index.html"));
                assert!(fs.exists("/css/style.css"));
                assert!(fs.exists("/js/app.js"));
                // No ignore rules — everything included
                assert!(fs.exists("/.hidden"));
                assert!(fs.exists("/build/output.bin"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn simple_pack_preserves_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        let mut image = LfsImage::new(test_image_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                pack_err(pack_directory_simple(fs, dir.path(), ""))?;
                let data = fs.read_file("/test.txt")?;
                assert_eq!(data, b"hello world");
                Ok(())
            })
            .unwrap();
    }
}
