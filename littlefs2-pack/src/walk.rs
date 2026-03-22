use std::path::{Path, PathBuf};

use crate::config::DirectoryConfig;
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WalkError {
    #[error("directory walk error: {0}")]
    Walk(#[from] ignore::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("path is not valid UTF-8: {}", .0.display())]
    InvalidPath(PathBuf),
}

/// The LFS paths collected during a [`pack_directory`] call.
///
/// Both vectors are sorted for deterministic output. Directory paths
/// include the leading `/` (e.g. `/config`), and file paths include
/// the full LFS path (e.g. `/config/network.json`).
#[derive(Clone, Debug, Default)]
pub(crate) struct PathSet {
    /// The root relative path
    pub root: PathBuf,
    /// Directories created in the image (e.g. `"/config"`).
    pub dirs: Vec<String>,
    /// Files written to the image (e.g. `"/config/network.json"`).
    pub files: Vec<String>,
}

impl PathSet {
    fn new_at(root: PathBuf) -> Self {
        PathSet {
            dirs: Vec::new(),
            files: Vec::new(),
            root,
        }
    }

    fn contains(&self, path: &str) -> bool {
        let path_string: String = path.to_string();
        self.dirs.contains(&path_string) || self.files.contains(&path_string)
    }

    fn sort(&mut self) {
        self.dirs.sort();
        self.files.sort();
    }

    pub fn host_path(&self, lfs_path: &str) -> PathBuf {
        self.root.join(lfs_path.trim_start_matches('/'))
    }
}

/// Build a `WalkBuilder` from a `DirectoryConfig`.
///
/// Applies the depth, hidden-file, gitignore, and glob settings
/// from the TOML configuration.
pub(crate) fn walker(config: &DirectoryConfig) -> WalkBuilder {
    let root = &config.resolved_root;
    let mut builder = WalkBuilder::new(root);
    builder.hidden(config.ignore_hidden);

    let depth = config.depth;
    if depth >= 0 {
        builder.max_depth(Some(depth as usize));
    }

    builder
        .git_ignore(config.gitignore)
        .git_global(config.repo_gitignore);

    let mut overrides = OverrideBuilder::new(root);

    // Negate patterns to ignore them
    for pattern in &config.glob_ignores {
        overrides
            .add(&format!("!{pattern}"))
            .expect("glob patterns are validated when DirectoryConfig is created");
    }

    // Include patterns override ignores — added after so they win
    for pattern in &config.glob_includes {
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
fn to_lfs_path(host_path: &Path, root: &Path) -> Result<String, WalkError> {
    let relative = host_path
        .strip_prefix(root)
        .map_err(|_| WalkError::InvalidPath(host_path.to_owned()))?;

    let s = relative
        .to_str()
        .ok_or_else(|| WalkError::InvalidPath(host_path.to_owned()))?;

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
pub(crate) fn walk_directory(config: &DirectoryConfig) -> Result<PathSet, WalkError> {
    let root = &config.resolved_root;
    let walk = walker(config);

    let mut to_pack = PathSet::new_at(root.clone());
    let mut seen = PathSet::new_at(root.clone());

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
            .ok_or_else(|| WalkError::InvalidPath(entry.path().to_owned()))?;

        let lfs_path = to_lfs_path(entry.path(), root)?;

        if ft.is_dir() {
            seen.dirs.push(lfs_path.clone());
            to_pack.dirs.push(lfs_path);
        } else if ft.is_file() {
            seen.files.push(lfs_path.clone());
            to_pack.files.push(lfs_path);
        }
    }

    // Rescue walk: pick up files matching glob_includes that the main
    // walk skipped (because of hidden-file rules, gitignore, or glob_ignores).
    if let Some(include_set) = &config.include_set {
        let mut rescue = WalkBuilder::new(root);
        rescue
            .hidden(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);

        let depth = config.depth;
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
                .ok_or_else(|| WalkError::InvalidPath(entry.path().to_owned()))?;

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
                seen.dirs.push(lfs_path.clone());
                to_pack.dirs.push(lfs_path);
            } else if ft.is_file() {
                // Ensure parent directories of rescued files are created.
                // The parent might have been skipped by the main walk
                // (e.g. a hidden directory containing a rescued file).
                if let Some(parent) = entry.path().parent() {
                    if parent != root.as_path() {
                        let parent_lfs = to_lfs_path(parent, root)?;
                        if !seen.contains(&parent_lfs) {
                            seen.files.push(parent_lfs.clone());
                            to_pack.dirs.push(parent_lfs);
                        }
                    }
                }
                seen.files.push(lfs_path.clone());
                to_pack.files.push(lfs_path);
            }
        }
    }

    // Sort for deterministic packing
    to_pack.sort();
    Ok(to_pack)
}

/// Simple recursive directory packing without ignore/glob rules.
/// Used when no TOML config is provided.
pub(crate) fn walk_directory_simple(root: &Path) -> Result<PathSet, WalkError> {
    let mut to_pack = PathSet::new_at(root.to_owned());
    walk_recursive(root, root, &mut to_pack)?;
    Ok(to_pack)
}

fn walk_recursive(dir: &Path, root: &Path, to_pack: &mut PathSet) -> Result<(), WalkError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let file_type = entry.file_type()?;
        let path = entry.path();
        let lfs_path = to_lfs_path(&path, root)?;

        if file_type.is_dir() {
            to_pack.dirs.push(lfs_path);
            walk_recursive(&path, root, to_pack)?;
        } else if file_type.is_file() {
            to_pack.files.push(lfs_path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DirectoryConfig;
    use globset::{Glob, GlobSetBuilder};
    use std::fs;

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// Build a DirectoryConfig directly from parameters.
    fn make_dir_config(
        root: &Path,
        depth: i32,
        ignore_hidden: bool,
        glob_ignores: &[&str],
        glob_includes: &[&str],
    ) -> DirectoryConfig {
        let glob_ignores: Vec<String> = glob_ignores.iter().map(|s| s.to_string()).collect();
        let glob_includes: Vec<String> = glob_includes.iter().map(|s| s.to_string()).collect();

        let include_set = if glob_includes.is_empty() {
            None
        } else {
            let mut builder = GlobSetBuilder::new();
            for pattern in &glob_includes {
                builder.add(Glob::new(pattern).unwrap());
            }
            Some(builder.build().unwrap())
        };

        DirectoryConfig {
            resolved_root: root.to_owned(),
            depth,
            ignore_hidden,
            gitignore: false,
            repo_gitignore: false,
            glob_ignores,
            glob_includes,
            include_set,
        }
    }

    fn default_dir_config(root: &Path) -> DirectoryConfig {
        make_dir_config(root, -1, true, &[], &[])
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
        assert!(matches!(err, WalkError::InvalidPath(_)));
    }

    // -------------------------------------------------------------------------
    // walker: hidden files
    // -------------------------------------------------------------------------

    #[test]
    fn walker_hides_hidden_files() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = default_dir_config(dir.path());
        let files = walk_file_names(walker(&config));

        assert!(!files.contains(&".hidden".to_string()));
        assert!(files.contains(&"index.html".to_string()));
    }

    #[test]
    fn walker_shows_hidden_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = make_dir_config(dir.path(), -1, false, &[], &[]);
        let files = walk_file_names(walker(&config));

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

        let config = make_dir_config(root, 2, true, &[], &[]);
        let files = walk_file_names(walker(&config));

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

        let config = default_dir_config(root);
        let files = walk_file_names(walker(&config));

        assert!(files.contains(&"deep.txt".to_string()));
    }

    // -------------------------------------------------------------------------
    // walker: glob ignores
    // -------------------------------------------------------------------------

    #[test]
    fn walker_glob_ignores_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = make_dir_config(dir.path(), -1, true, &["*.bin"], &[]);
        let files = walk_file_names(walker(&config));

        assert!(!files.contains(&"output.bin".to_string()));
        assert!(files.contains(&"index.html".to_string()));
    }

    #[test]
    fn walker_glob_ignores_directory() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = make_dir_config(dir.path(), -1, true, &["build"], &[]);
        let all_paths: Vec<PathBuf> = walker(&config)
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
        let config = make_dir_config(root, -1, false, &["*.bin"], &["keep.bin"]);
        let files = walk_file_names(walker(&config));

        assert!(files.contains(&"keep.bin".to_string()));
        assert!(!files.contains(&"drop.bin".to_string()));
        assert!(!files.contains(&"also.txt".to_string()));
    }

    // -------------------------------------------------------------------------
    // walk_directory: returns correct structure
    // -------------------------------------------------------------------------

    #[test]
    fn walk_returns_correct_structure() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = default_dir_config(dir.path());
        let paths = walk_directory(&config).unwrap();

        assert!(paths.files.contains(&"/index.html".to_string()));
        assert!(paths.files.contains(&"/css/style.css".to_string()));
        assert!(paths.files.contains(&"/js/app.js".to_string()));

        assert!(paths.dirs.contains(&"/css".to_string()));
        assert!(paths.dirs.contains(&"/js".to_string()));
        assert!(paths.dirs.contains(&"/build".to_string()));
    }

    #[test]
    fn walk_stores_root() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = default_dir_config(dir.path());
        let paths = walk_directory(&config).unwrap();

        assert_eq!(paths.root, dir.path());
    }

    #[test]
    fn walk_respects_hidden_ignore() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = default_dir_config(dir.path());
        let paths = walk_directory(&config).unwrap();

        assert!(!paths.files.contains(&"/.hidden".to_string()));
        assert!(paths.files.contains(&"/index.html".to_string()));
    }

    #[test]
    fn walk_includes_hidden_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = make_dir_config(dir.path(), -1, false, &[], &[]);
        let paths = walk_directory(&config).unwrap();

        assert!(paths.files.contains(&"/.hidden".to_string()));
    }

    #[test]
    fn walk_with_glob_ignores() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = make_dir_config(dir.path(), -1, true, &["build"], &[]);
        let paths = walk_directory(&config).unwrap();

        assert!(!paths.dirs.contains(&"/build".to_string()));
        assert!(!paths.files.contains(&"/build/output.bin".to_string()));
        assert!(paths.files.contains(&"/index.html".to_string()));
    }

    #[test]
    fn walk_empty_directory() {
        let dir = tempfile::tempdir().unwrap();

        let config = default_dir_config(dir.path());
        let paths = walk_directory(&config).unwrap();

        assert!(paths.dirs.is_empty());
        assert!(paths.files.is_empty());
    }

    // -------------------------------------------------------------------------
    // walk_directory: deterministic output
    // -------------------------------------------------------------------------

    #[test]
    fn walk_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = default_dir_config(dir.path());

        let first = walk_directory(&config).unwrap();
        let second = walk_directory(&config).unwrap();

        assert_eq!(first.dirs, second.dirs);
        assert_eq!(first.files, second.files);
    }

    // -------------------------------------------------------------------------
    // walk_directory_simple
    // -------------------------------------------------------------------------

    #[test]
    fn simple_walk_includes_everything() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let paths = walk_directory_simple(dir.path()).unwrap();

        assert!(paths.files.contains(&"/index.html".to_string()));
        assert!(paths.files.contains(&"/css/style.css".to_string()));
        assert!(paths.files.contains(&"/js/app.js".to_string()));
        // No ignore rules — everything included
        assert!(paths.files.contains(&"/.hidden".to_string()));
        assert!(paths.files.contains(&"/build/output.bin".to_string()));
    }

    #[test]
    fn simple_walk_includes_directories() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let paths = walk_directory_simple(dir.path()).unwrap();

        assert!(paths.dirs.contains(&"/css".to_string()));
        assert!(paths.dirs.contains(&"/js".to_string()));
        assert!(paths.dirs.contains(&"/build".to_string()));
    }

    #[test]
    fn simple_walk_stores_root() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let paths = walk_directory_simple(dir.path()).unwrap();

        assert_eq!(paths.root, dir.path());
    }

    #[test]
    fn simple_walk_deeply_nested() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("a/b/c/deep.txt"), "deep").unwrap();
        fs::write(root.join("a/top.txt"), "top").unwrap();

        let paths = walk_directory_simple(root).unwrap();

        assert!(paths.dirs.contains(&"/a".to_string()));
        assert!(paths.dirs.contains(&"/a/b".to_string()));
        assert!(paths.dirs.contains(&"/a/b/c".to_string()));
        assert!(paths.files.contains(&"/a/top.txt".to_string()));
        assert!(paths.files.contains(&"/a/b/c/deep.txt".to_string()));
    }

    #[test]
    fn simple_walk_empty_directory() {
        let dir = tempfile::tempdir().unwrap();

        let paths = walk_directory_simple(dir.path()).unwrap();

        assert!(paths.dirs.is_empty());
        assert!(paths.files.is_empty());
    }

    // -------------------------------------------------------------------------
    // PathSet::host_path
    // -------------------------------------------------------------------------

    #[test]
    fn host_path_resolves_root_file() {
        let paths = PathSet::new_at(PathBuf::from("/tmp/site"));

        assert_eq!(
            paths.host_path("/index.html"),
            PathBuf::from("/tmp/site/index.html")
        );
    }

    #[test]
    fn host_path_resolves_nested_file() {
        let paths = PathSet::new_at(PathBuf::from("/tmp/site"));

        assert_eq!(
            paths.host_path("/css/style.css"),
            PathBuf::from("/tmp/site/css/style.css")
        );
    }

    #[test]
    fn host_path_roundtrips_through_walk() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let config = default_dir_config(dir.path());
        let paths = walk_directory(&config).unwrap();

        // Every file in the walk result should resolve to a host path that exists
        for lfs_path in &paths.files {
            let host = paths.host_path(lfs_path);
            assert!(host.exists(), "host path should exist: {}", host.display());
        }
        for lfs_path in &paths.dirs {
            let host = paths.host_path(lfs_path);
            assert!(
                host.is_dir(),
                "host path should be a dir: {}",
                host.display()
            );
        }
    }

    #[test]
    fn host_path_roundtrips_through_simple_walk() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let paths = walk_directory_simple(dir.path()).unwrap();

        for lfs_path in &paths.files {
            let host = paths.host_path(lfs_path);
            assert!(host.exists(), "host path should exist: {}", host.display());
        }
        for lfs_path in &paths.dirs {
            let host = paths.host_path(lfs_path);
            assert!(
                host.is_dir(),
                "host path should be a dir: {}",
                host.display()
            );
        }
    }
}
