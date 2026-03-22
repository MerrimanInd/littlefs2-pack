use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use littlefs2_pack::config::{Config, ImageConfig, RawImageConfig};
use littlefs2_pack::littlefs::{LfsError, LfsImage, MountedFs};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "littlefs",
    version,
    about = "Create, unpack, and inspect LittleFSv2 filesystem images"
)]
pub struct Cli {
    /// Path to a littlefs.toml configuration file
    #[arg(long, short = 'f', global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Pack a directory into a LittleFS2 image
    Pack(PackCmd),
    /// Unpack a LittleFS2 image into a directory
    Unpack(UnpackCmd),
    /// List files in a LittleFS2 image
    List(ListCmd),
    /// Print info about a LittleFS2 image (block count, used space, etc.)
    Info(InfoCmd),
    /// Run the flash commands from a TOML config file
    Flash(FlashCmd),
}

// ---------------------------------------------------------------------------
// Shared filesystem parameters (flattened into each subcommand)
// ---------------------------------------------------------------------------

/// LittleFS2 filesystem geometry parameters.
///
/// When used with `--config`, these override the TOML values.
/// Without `--config`, these define the image parameters directly.
#[derive(Args, Debug, Clone)]
pub struct ImageConfigParams {
    /// Filesystem block (erase unit) size in bytes.
    #[arg(short, long)]
    pub block_size: Option<usize>,

    /// Total number of blocks in the filesystem.
    #[arg(short = 'c', long, conflicts_with = "image_size")]
    pub block_count: Option<usize>,

    /// Total image size in bytes (alternative to --block-count).
    #[arg(short = 's', long, conflicts_with = "block_count")]
    pub image_size: Option<usize>,

    /// Page size in bytes. Sets both read and write size if they are not given.
    #[arg(short, long)]
    pub page_size: Option<usize>,

    /// Minimum read size in bytes (overrides --page-size for reads).
    #[arg(long)]
    pub read_size: Option<usize>,

    /// Minimum program (write) size in bytes (overrides --page-size for writes).
    #[arg(long)]
    pub write_size: Option<usize>,

    /// Block-cycle count for wear leveling (-1 disables).
    #[arg(long, allow_hyphen_values = true)]
    pub block_cycles: Option<i32>,

    /// Cache size in bytes (must be multiple of read_size and write_size,
    /// and must evenly divide block_size). Defaults to max(read_size, write_size).
    #[arg(long)]
    pub cache_size: Option<usize>,

    /// Lookahead buffer size in bytes (must be a multiple of 8).
    /// Defaults to the smallest valid value that covers all blocks.
    #[arg(long)]
    pub lookahead_size: Option<usize>,
}

// ---------------------------------------------------------------------------
// Config resolution: TOML + CLI overrides
// ---------------------------------------------------------------------------

/// Build an `ImageConfig` entirely from CLI arguments using the builder pattern.
fn image_config_from_cli(cli: &ImageConfigParams) -> Result<ImageConfig> {
    let block_size = match cli.block_size {
        Some(bs) => bs,
        None => bail!("--block-size is required without --config"),
    };

    let mut builder = RawImageConfig::new()
        .with_block_size(block_size)
        .with_block_cycles(cli.block_cycles.unwrap_or(-1));

    if let Some(c) = cli.block_count {
        builder = builder.with_block_count(c);
    }
    if let Some(s) = cli.image_size {
        builder = builder.with_image_size(s);
    }
    if let Some(p) = cli.page_size {
        builder = builder.with_page_size(p);
    }
    if let Some(r) = cli.read_size {
        builder = builder.with_read_size(r);
    }
    if let Some(w) = cli.write_size {
        builder = builder.with_write_size(w);
    }
    if let Some(c) = cli.cache_size {
        builder = builder.with_cache_size(c);
    }
    if let Some(l) = cli.lookahead_size {
        builder = builder.with_lookahead_size(l);
    }

    Ok(builder.resolve()?)
}

/// Apply CLI overrides to an `ImageConfig` loaded from TOML.
///
/// Starts from the TOML values, then overwrites anything the user
/// explicitly passed on the command line.
fn apply_cli_overrides(base: &ImageConfig, cli: &ImageConfigParams) -> ImageConfig {
    let mut builder = RawImageConfig::new()
        .with_block_size(cli.block_size.unwrap_or(base.block_size))
        .with_read_size(cli.read_size.unwrap_or(base.read_size))
        .with_write_size(cli.write_size.unwrap_or(base.write_size))
        .with_block_cycles(cli.block_cycles.unwrap_or(base.block_cycles));

    // Only carry forward the TOML's cache_size if the CLI didn't change any
    // of the values it depends on (read_size, write_size, block_size).
    // Otherwise let resolve() recompute a valid default.
    if let Some(c) = cli.cache_size {
        builder = builder.with_cache_size(c);
    } else if cli.read_size.is_none() && cli.write_size.is_none() && cli.block_size.is_none() {
        builder = builder.with_cache_size(base.cache_size);
    }

    // Same for lookahead_size: carry forward only if block_count didn't change.
    if let Some(l) = cli.lookahead_size {
        builder = builder.with_lookahead_size(l);
    } else if cli.block_count.is_none() && cli.image_size.is_none() {
        builder = builder.with_lookahead_size(base.lookahead_size);
    }

    // If the user passed --image-size, use that instead of the TOML's block_count
    if let Some(s) = cli.image_size {
        builder = builder.with_image_size(s);
    } else {
        builder = builder.with_block_count(cli.block_count.unwrap_or(base.block_count));
    }

    builder
        .resolve()
        .expect("CLI overrides produced an invalid configuration")
}

/// Resolve an `ImageConfig` for reading an existing image file.
///
/// The block count is derived from the file size, since the file
/// is the source of truth for how large the image is.
fn image_config_for_reading(
    config_path: &Option<PathBuf>,
    cli: &ImageConfigParams,
    data: &[u8],
) -> Result<ImageConfig> {
    // Get block_size and read/write sizes from TOML or CLI
    let (block_size, read_size, write_size, block_cycles, cache_size, lookahead_size) =
        match config_path {
            Some(path) => {
                let config = Config::from_file(path)?;
                (
                    cli.block_size.unwrap_or(config.image.block_size),
                    cli.read_size.unwrap_or(config.image.read_size),
                    cli.write_size.unwrap_or(config.image.write_size),
                    cli.block_cycles.unwrap_or(config.image.block_cycles),
                    cli.cache_size.or(Some(config.image.cache_size)),
                    cli.lookahead_size.or(Some(config.image.lookahead_size)),
                )
            }
            None => {
                let block_size = match cli.block_size {
                    Some(bs) => bs,
                    None => bail!("--block-size is required without --config"),
                };
                let read_size = match cli.read_size.or(cli.page_size) {
                    Some(rs) => rs,
                    None => bail!("--page-size or --read-size required without --config"),
                };
                let write_size = match cli.write_size.or(cli.page_size) {
                    Some(ws) => ws,
                    None => bail!("--page-size or --write-size required without --config"),
                };
                (
                    block_size,
                    read_size,
                    write_size,
                    cli.block_cycles.unwrap_or(-1),
                    cli.cache_size,
                    cli.lookahead_size,
                )
            }
        };

    if data.is_empty() || data.len() % block_size != 0 {
        bail!(
            "image file size ({}) is not a multiple of block_size ({block_size})",
            data.len()
        );
    }

    let block_count = data.len() / block_size;

    // Build through RawImageConfig so defaults and validation are applied
    let mut builder = RawImageConfig::new()
        .with_block_size(block_size)
        .with_block_count(block_count)
        .with_read_size(read_size)
        .with_write_size(write_size)
        .with_block_cycles(block_cycles);

    if let Some(c) = cache_size {
        builder = builder.with_cache_size(c);
    }
    if let Some(l) = lookahead_size {
        builder = builder.with_lookahead_size(l);
    }

    Ok(builder.resolve()?)
}

// ---------------------------------------------------------------------------
// Subcommand argument structs
// ---------------------------------------------------------------------------

/// Arguments for the `pack` subcommand.
#[derive(Args)]
pub struct PackCmd {
    /// Source directory to pack (overrides TOML [directory] root)
    #[arg(short = 'd', long)]
    pub pack_directory: Option<PathBuf>,

    /// Output image file path
    #[arg(short, long)]
    pub output: PathBuf,

    #[command(flatten)]
    pub fs: ImageConfigParams,
}

/// Arguments for the `unpack` subcommand.
#[derive(Args)]
pub struct UnpackCmd {
    /// LittleFS2 image file to unpack
    #[arg(short, long)]
    pub image: PathBuf,

    /// Output directory
    #[arg(short = 'd', long)]
    pub unpack_directory: PathBuf,

    #[command(flatten)]
    pub fs: ImageConfigParams,
}

/// Arguments for the `list` subcommand.
#[derive(Args)]
pub struct ListCmd {
    /// LittleFS2 image file to inspect
    #[arg(short, long)]
    pub image: PathBuf,

    #[command(flatten)]
    pub fs: ImageConfigParams,
}

/// Arguments for the `info` subcommand.
#[derive(Args)]
pub struct InfoCmd {
    /// LittleFS2 image file to inspect
    #[arg(short, long)]
    pub image: PathBuf,

    #[command(flatten)]
    pub fs: ImageConfigParams,
}

/// Arguments for the `flash` subcommand.
#[derive(Args)]
pub struct FlashCmd {
    /// Path to the firmware binary. Passed automatically by `cargo run`,
    /// or falls back to the `path` field in `[flash.firmware]`.
    #[arg(value_name = "BINARY")]
    pub binary_path: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Pack(args) => cmd_pack(&cli.config, args)?,
        Commands::Unpack(args) => cmd_unpack(&cli.config, args)?,
        Commands::List(args) => cmd_list(&cli.config, args)?,
        Commands::Info(args) => cmd_info(&cli.config, args)?,
        Commands::Flash(args) => cmd_flash(&cli.config, args)?,
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// pack
// ---------------------------------------------------------------------------

/// Pack a local directory tree into a LittleFS2 image file.
///
/// When a TOML config is provided, image geometry and directory settings
/// are loaded from it with any CLI flags applied as overrides. Without
/// a config, all parameters must be supplied on the command line.
fn cmd_pack(config_path: &Option<PathBuf>, args: PackCmd) -> Result<()> {
    // Resolve everything from TOML + CLI overrides
    let (image_config, root, directory_config) = match config_path {
        Some(path) => {
            let config = Config::from_file(path)?;
            let image_config = apply_cli_overrides(&config.image, &args.fs);
            let mut dir_config = config.directory;
            // CLI --pack-directory overrides the TOML root
            if let Some(d) = args.pack_directory {
                dir_config.resolved_root = d;
            }
            let root = dir_config.resolved_root.clone();
            (image_config, root, Some(dir_config))
        }
        None => {
            let image_config = image_config_from_cli(&args.fs)?;
            let root = match args.pack_directory {
                Some(d) => d,
                None => bail!("--pack-directory is required without --config"),
            };
            (image_config, root, None)
        }
    };

    let block_count = image_config.block_count;
    let block_size = image_config.block_size;

    let mut image = LfsImage::new(image_config)?;
    image.format()?;

    match directory_config {
        Some(dir_config) => image.pack_from_config(dir_config)?,
        None => image.pack_from_dir(&root)?,
    };

    let data = image.into_data();
    std::fs::write(&args.output, &data)
        .with_context(|| format!("failed to write image to '{}'", args.output.display()))?;

    println!(
        "Packed '{}' -> '{}' ({} bytes, {} blocks x {} bytes)",
        root.display(),
        args.output.display(),
        data.len(),
        block_count,
        block_size,
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared: load an existing image
// ---------------------------------------------------------------------------

/// Read an existing image file from disk and wrap it in an [`LfsImage`].
///
/// Image geometry is resolved from the TOML config and/or CLI flags,
/// with the block count derived from the actual file size.
fn load_image(
    config_path: &Option<PathBuf>,
    cli: &ImageConfigParams,
    image_path: &Path,
) -> Result<LfsImage> {
    let data = std::fs::read(image_path)
        .with_context(|| format!("failed to read image '{}'", image_path.display()))?;
    let config = image_config_for_reading(config_path, cli, &data)?;
    Ok(LfsImage::from_data(config, data)?)
}

// ---------------------------------------------------------------------------
// unpack
// ---------------------------------------------------------------------------

/// Extract all files and directories from a LittleFS2 image to a host directory.
fn cmd_unpack(config_path: &Option<PathBuf>, args: UnpackCmd) -> Result<()> {
    let mut image = load_image(config_path, &args.fs, &args.image)?;

    std::fs::create_dir_all(&args.unpack_directory)
        .with_context(|| format!("failed to create '{}'", args.unpack_directory.display()))?;

    image.mount_and_then(|fs| unpack_directory(fs, "/", &args.unpack_directory))?;

    println!(
        "Unpacked '{}' -> '{}'",
        args.image.display(),
        args.unpack_directory.display()
    );

    Ok(())
}

/// Recursively extract a single LFS directory and its children to the host filesystem.
fn unpack_directory(fs: &MountedFs<'_>, lfs_dir: &str, host_dir: &Path) -> Result<(), LfsError> {
    let entries = fs.read_dir(lfs_dir)?;

    for entry in entries {
        let host_path = host_dir.join(&entry.name);
        let lfs_child = if lfs_dir == "/" {
            format!("/{}", entry.name)
        } else {
            format!("{}/{}", lfs_dir, entry.name)
        };

        if entry.is_dir {
            std::fs::create_dir_all(&host_path)?;
            unpack_directory(fs, &lfs_child, &host_path)?;
        } else {
            let data = fs.read_file(&lfs_child)?;
            std::fs::write(&host_path, &data)?;
            println!("  extract {} ({} bytes)", host_path.display(), data.len());
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// Print a tree-style listing of every file and directory in a LittleFS2 image.
fn cmd_list(config_path: &Option<PathBuf>, args: ListCmd) -> Result<()> {
    let mut image = load_image(config_path, &args.fs, &args.image)?;

    image.mount_and_then(|fs| {
        println!("/");
        list_directory(fs, "/", "")
    })?;

    Ok(())
}

/// Recursively print a single LFS directory using Unicode tree connectors.
fn list_directory(fs: &MountedFs<'_>, lfs_dir: &str, prefix: &str) -> Result<(), LfsError> {
    let entries = fs.read_dir(lfs_dir)?;
    let count = entries.len();

    for (i, entry) in entries.iter().enumerate() {
        let is_last = i == count - 1;
        let connector = if is_last { "╰── " } else { "├── " };
        let child_prefix = if is_last { "    " } else { "│   " };

        if entry.is_dir {
            println!("{prefix}{connector}{}/ ", entry.name);
            let sub = if lfs_dir == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{lfs_dir}/{}", entry.name)
            };
            let next_prefix = format!("{prefix}{child_prefix}");
            list_directory(fs, &sub, &next_prefix)?;
        } else {
            println!("{prefix}{connector}{} ({} bytes)", entry.name, entry.size);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

/// Print block usage statistics for a LittleFS2 image.
fn cmd_info(config_path: &Option<PathBuf>, args: InfoCmd) -> Result<()> {
    let mut image = load_image(config_path, &args.fs, &args.image)?;

    let bc = image.config().block_count;
    let bs = image.config().block_size;

    image.mount_and_then(|fs| {
        let used = fs.used_blocks()?;
        let free = bc.saturating_sub(used);

        println!("Image size:   {} bytes", bc * bs);
        println!("Block size:   {} bytes", bs);
        println!("Block count:  {}", bc);
        println!("Blocks used:  {} ({} bytes)", used, used * bs);
        println!("Blocks free:  {} ({} bytes)", free, free * bs);
        Ok(())
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// flash
// ---------------------------------------------------------------------------

/// Return the path where the SHA-256 hash of a flashed file is cached.
///
/// Hashes are stored under `target/.flash-cache/` so they are cleaned
/// by `cargo clean` and already covered by `.gitignore`.
fn cache_path(file: &Path) -> PathBuf {
    Path::new("target/.flash-cache").join(format!(
        "{}.sha256",
        file.file_name().unwrap().to_string_lossy()
    ))
}

/// Returns `Some(hash)` if the file needs flashing, `None` if unchanged.
fn needs_flash(file: &Path) -> Result<Option<String>> {
    let bytes =
        std::fs::read(file).with_context(|| format!("failed to read {}", file.display()))?;
    let hash = format!("{:x}", Sha256::digest(&bytes));

    let cache = cache_path(file);
    if cache.exists() {
        let prev = std::fs::read_to_string(&cache).unwrap_or_default();
        if prev.trim() == hash {
            return Ok(None);
        }
    }
    Ok(Some(hash))
}

/// Record that `file` was successfully flashed by writing its hash to the cache.
///
/// Called *after* a successful flash so that an interrupted flash will
/// be retried on the next run.
fn mark_flashed(file: &Path, hash: &str) -> Result<()> {
    let cache = cache_path(file);
    std::fs::create_dir_all(cache.parent().unwrap())?;
    std::fs::write(&cache, hash)?;
    Ok(())
}

/// Flash firmware and filesystem image to a device.
///
/// Intended to be invoked as a Cargo runner (`runner = "littlefs flash"`
/// in `.cargo/config.toml`) so that `cargo run` triggers a full
/// flash cycle. The sequence is:
///
/// 1. Flash the LittleFS filesystem image (skipped if unchanged).
/// 2. Flash the firmware binary (always, to preserve normal `cargo run`
///    behaviour — the firmware command may also start a monitor session).
///
/// The filesystem is flashed first because the firmware flash typically
/// resets the chip and starts execution; the FS must be in place before
/// the application boots.
fn cmd_flash(config_path: &Option<PathBuf>, args: FlashCmd) -> Result<()> {
    // Load the config
    let config_path = config_path
        .as_ref()
        .context("no project config file path handed in!")?;

    let config = Config::from_file(config_path).context("failed to load config file")?;
    let flash_config = config
        .flash
        .context("project config file must have [flash] section defined!")?;

    // Resolve the firmware binary path: CLI arg (from `cargo run`) takes
    // priority, then the hardcoded path in [flash.firmware].
    let binary_path = args
        .binary_path
        .or(flash_config.firmware.path)
        .context("no firmware path (pass as argument or set path in [flash.firmware])")?;

    // Resolve the filesystem image path: explicit config path takes priority,
    // otherwise derive it from the binary's parent directory (where build.rs
    // copies it). Only accept the derived path if the file actually exists.
    let image_path = flash_config
        .filesystem
        .path
        .or_else(|| {
            binary_path
                .parent()
                .map(|dir| dir.join(format!("{}.bin", &config.image.name)))
                .filter(|p| p.exists())
        })
        .context(
            "no filesystem image found (set path in [flash.filesystem] \
         or run via `cargo run` so the image can be located automatically)",
        )?;

    // Flash the filesystem image, but only if it has changed since the last
    // flash. This avoids unnecessary wear on flash memory for assets that
    // are typically more static than the firmware itself.
    if let Some(hash) = needs_flash(&image_path)? {
        println!("Flashing filesystem image...");
        run_command(
            &flash_config.filesystem.command,
            &[
                (
                    "path",
                    image_path.to_str().context("invalid image file path")?,
                ),
                ("address", flash_config.filesystem.address.as_str()),
            ],
        )
        .context("failed to write filesystem binary")?;
        mark_flashed(&image_path, &hash)?;
    } else {
        println!("Filesystem image unchanged, skipping.");
    }

    // Always flash the firmware to match normal `cargo run` behaviour.
    // This command may also reset the chip and start a monitor session,
    // so it must be the last step.
    run_command(
        &flash_config.firmware.command,
        &[(
            "path",
            binary_path.to_str().context("invalid binary file path")?,
        )],
    )
    .context("failed to flash binary")?;

    Ok(())
}

/// Expand a command template and execute it as a subprocess.
///
/// Placeholders like `{path}` and `{address}` in `template` are replaced
/// with the corresponding values from `vars`. The expanded string is then
/// split on whitespace and executed. Returns an error if the command is
/// not found or exits with a non-zero status.
fn run_command(template: &str, vars: &[(&str, &str)]) -> Result<()> {
    let mut expanded = template.to_string();
    for (key, value) in vars {
        expanded = expanded.replace(&format!("{{{key}}}"), value);
    }

    let parts: Vec<&str> = expanded.split_whitespace().collect();
    let (program, args) = parts.split_first().context("empty command")?;

    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run: {program}"))?;

    anyhow::ensure!(status.success(), "{program} exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use littlefs2_pack::config::{DEFAULT_IMAGE_NAME, ImageConfig};
    use std::fs;

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// Create an ImageConfigParams with all fields None.
    fn empty_cli() -> ImageConfigParams {
        ImageConfigParams {
            block_size: None,
            block_count: None,
            image_size: None,
            page_size: None,
            read_size: None,
            write_size: None,
            block_cycles: None,
            cache_size: None,
            lookahead_size: None,
        }
    }

    /// Write a minimal littlefs.toml and create the directory root it references.
    fn write_test_toml(dir: &Path, image_overrides: &str) -> PathBuf {
        let site_dir = dir.join("site");
        fs::create_dir_all(&site_dir).unwrap();

        let toml_path = dir.join("littlefs.toml");
        fs::write(
            &toml_path,
            format!(
                r#"
[image]
block_size = 4096
block_count = 128
page_size = 256
read_size = 16
write_size = 512
{image_overrides}

[directory]
root = "./site"
depth = -1
ignore_hidden = true
gitignore = false
repo_gitignore = false
glob_ignores = []
glob_includes = []
"#
            ),
        )
        .unwrap();

        toml_path
    }

    // -------------------------------------------------------------------------
    // image_config_from_cli: valid constructions
    // -------------------------------------------------------------------------

    #[test]
    fn cli_with_block_count_and_page_size() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            block_count: Some(64),
            page_size: Some(256),
            ..empty_cli()
        };
        let config = image_config_from_cli(&cli).unwrap();
        assert_eq!(config.block_size, 4096);
        assert_eq!(config.block_count, 64);
        assert_eq!(config.read_size, 256);
        assert_eq!(config.write_size, 256);
        assert_eq!(config.block_cycles, -1);
    }

    #[test]
    fn cli_with_image_size() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            image_size: Some(4096 * 64),
            page_size: Some(256),
            ..empty_cli()
        };
        let config = image_config_from_cli(&cli).unwrap();
        assert_eq!(config.block_count, 64);
        assert_eq!(config.image_size(), 4096 * 64);
    }

    #[test]
    fn cli_with_explicit_read_write_sizes() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            block_count: Some(64),
            read_size: Some(16),
            write_size: Some(512),
            ..empty_cli()
        };
        let config = image_config_from_cli(&cli).unwrap();
        assert_eq!(config.read_size, 16);
        assert_eq!(config.write_size, 512);
    }

    #[test]
    fn cli_read_write_override_page_size() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            block_count: Some(64),
            page_size: Some(256),
            read_size: Some(16),
            write_size: Some(512),
            ..empty_cli()
        };
        let config = image_config_from_cli(&cli).unwrap();
        assert_eq!(config.read_size, 16);
        assert_eq!(config.write_size, 512);
    }

    #[test]
    fn cli_explicit_block_cycles() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            block_count: Some(64),
            page_size: Some(256),
            block_cycles: Some(500),
            ..empty_cli()
        };
        let config = image_config_from_cli(&cli).unwrap();
        assert_eq!(config.block_cycles, 500);
    }

    #[test]
    fn cli_block_cycles_defaults_to_negative_one() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            block_count: Some(64),
            page_size: Some(256),
            ..empty_cli()
        };
        let config = image_config_from_cli(&cli).unwrap();
        assert_eq!(config.block_cycles, -1);
    }

    // -------------------------------------------------------------------------
    // image_config_from_cli: error cases
    // -------------------------------------------------------------------------

    #[test]
    fn cli_missing_block_size_fails() {
        let cli = ImageConfigParams {
            block_count: Some(64),
            page_size: Some(256),
            ..empty_cli()
        };
        assert!(image_config_from_cli(&cli).is_err());
    }

    #[test]
    fn cli_missing_page_and_read_write_fails() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            block_count: Some(64),
            ..empty_cli()
        };
        assert!(image_config_from_cli(&cli).is_err());
    }

    #[test]
    fn cli_missing_block_count_and_image_size_fails() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            page_size: Some(256),
            ..empty_cli()
        };
        assert!(image_config_from_cli(&cli).is_err());
    }

    // -------------------------------------------------------------------------
    // apply_cli_overrides
    // -------------------------------------------------------------------------

    fn test_base_config() -> ImageConfig {
        ImageConfig {
            name: DEFAULT_IMAGE_NAME.into(),
            block_size: 4096,
            block_count: 128,
            read_size: 256,
            write_size: 256,
            block_cycles: -1,
            cache_size: 256,
            lookahead_size: 16,
        }
    }

    #[test]
    fn overrides_no_cli_args_preserves_toml() {
        let base = ImageConfig {
            name: DEFAULT_IMAGE_NAME.into(),
            block_size: 4096,
            block_count: 128,
            read_size: 16,
            write_size: 512,
            block_cycles: -1,
            cache_size: 512,
            lookahead_size: 16,
        };
        let cli = empty_cli();

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_size, 4096);
        assert_eq!(config.block_count, 128);
        assert_eq!(config.read_size, 16);
        assert_eq!(config.write_size, 512);
        assert_eq!(config.cache_size, 512);
        assert_eq!(config.lookahead_size, 16);
    }

    #[test]
    fn overrides_block_size() {
        let base = test_base_config();
        let cli = ImageConfigParams {
            block_size: Some(512),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_size, 512);
        assert_eq!(config.block_count, 128); // preserved from TOML
    }

    #[test]
    fn overrides_block_count() {
        let base = test_base_config();
        let cli = ImageConfigParams {
            block_count: Some(64),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_count, 64);
    }

    #[test]
    fn overrides_image_size_replaces_block_count() {
        let base = test_base_config();
        let cli = ImageConfigParams {
            image_size: Some(4096 * 32),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_count, 32);
        assert_eq!(config.image_size(), 4096 * 32);
    }

    #[test]
    fn overrides_read_write_sizes() {
        let base = test_base_config();
        let cli = ImageConfigParams {
            read_size: Some(16),
            write_size: Some(512),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.read_size, 16);
        assert_eq!(config.write_size, 512);
    }

    #[test]
    fn overrides_block_cycles() {
        let base = test_base_config();
        let cli = ImageConfigParams {
            block_cycles: Some(100),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_cycles, 100);
    }

    // -------------------------------------------------------------------------
    // image_config_for_reading: with TOML
    // -------------------------------------------------------------------------

    #[test]
    fn reading_config_from_toml_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = write_test_toml(dir.path(), "");
        let config_path = Some(toml_path);

        // Simulate a 64-block image file
        let data = vec![0xFF; 4096 * 64];
        let config = image_config_for_reading(&config_path, &empty_cli(), &data).unwrap();

        // block_count comes from file size, not TOML
        assert_eq!(config.block_count, 64);
        // Other params come from TOML
        assert_eq!(config.block_size, 4096);
        assert_eq!(config.read_size, 16);
        assert_eq!(config.write_size, 512);
    }

    #[test]
    fn reading_config_cli_overrides_toml() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = write_test_toml(dir.path(), "");
        let config_path = Some(toml_path);

        let cli = ImageConfigParams {
            read_size: Some(32),
            ..empty_cli()
        };

        let data = vec![0xFF; 4096 * 64];
        let config = image_config_for_reading(&config_path, &cli, &data).unwrap();

        assert_eq!(config.read_size, 32);
        assert_eq!(config.write_size, 512); // from TOML
    }

    // -------------------------------------------------------------------------
    // image_config_for_reading: CLI only
    // -------------------------------------------------------------------------

    #[test]
    fn reading_config_cli_only() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            page_size: Some(256),
            ..empty_cli()
        };

        let data = vec![0xFF; 4096 * 32];
        let config = image_config_for_reading(&None, &cli, &data).unwrap();

        assert_eq!(config.block_size, 4096);
        assert_eq!(config.block_count, 32);
        assert_eq!(config.read_size, 256);
        assert_eq!(config.write_size, 256);
    }

    #[test]
    fn reading_config_misaligned_file_fails() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            page_size: Some(256),
            ..empty_cli()
        };

        let data = vec![0xFF; 5000]; // not a multiple of 4096
        assert!(image_config_for_reading(&None, &cli, &data).is_err());
    }

    #[test]
    fn reading_config_empty_file_fails() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            page_size: Some(256),
            ..empty_cli()
        };

        let data = vec![];
        assert!(image_config_for_reading(&None, &cli, &data).is_err());
    }

    #[test]
    fn reading_config_cli_missing_block_size_fails() {
        let cli = ImageConfigParams {
            page_size: Some(256),
            ..empty_cli()
        };

        let data = vec![0xFF; 4096 * 32];
        assert!(image_config_for_reading(&None, &cli, &data).is_err());
    }

    #[test]
    fn reading_config_cli_missing_sizes_fails() {
        let cli = ImageConfigParams {
            block_size: Some(4096),
            ..empty_cli()
        };

        let data = vec![0xFF; 4096 * 32];
        assert!(image_config_for_reading(&None, &cli, &data).is_err());
    }

    // -------------------------------------------------------------------------
    // apply_cli_overrides: cache_size invalidation
    // -------------------------------------------------------------------------

    #[test]
    fn overrides_cache_size_recomputed_when_read_size_changes() {
        // base: read=256, write=256, cache=256
        let base = test_base_config();
        let cli = ImageConfigParams {
            read_size: Some(512),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        // cache_size should be recomputed as max(new_read, old_write) = max(512, 256) = 512
        assert_eq!(config.read_size, 512);
        assert_eq!(config.cache_size, 512);
    }

    #[test]
    fn overrides_cache_size_recomputed_when_write_size_changes() {
        let base = test_base_config();
        let cli = ImageConfigParams {
            write_size: Some(512),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.write_size, 512);
        assert_eq!(config.cache_size, 512);
    }

    #[test]
    fn overrides_cache_size_recomputed_when_block_size_changes() {
        // Use a base where cache_size (4096) differs from the default that
        // resolve() would pick (max(read=256, write=256) = 256), so we can
        // tell whether the value was carried forward or recomputed.
        let base = ImageConfig {
            name: DEFAULT_IMAGE_NAME.into(),
            block_size: 4096,
            block_count: 128,
            read_size: 256,
            write_size: 256,
            block_cycles: -1,
            cache_size: 4096,
            lookahead_size: 16,
        };
        let cli = ImageConfigParams {
            block_size: Some(512),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        // cache_size recomputed as max(256, 256) = 256, NOT carried forward as 4096
        // (which would be invalid: 512 % 4096 != 0)
        assert_eq!(config.cache_size, 256);
    }

    #[test]
    fn overrides_cache_size_preserved_when_unrelated_fields_change() {
        // When only block_count changes, cache_size should carry forward
        let base = ImageConfig {
            name: DEFAULT_IMAGE_NAME.into(),
            block_size: 4096,
            block_count: 128,
            read_size: 16,
            write_size: 512,
            block_cycles: -1,
            cache_size: 512,
            lookahead_size: 16,
        };
        let cli = ImageConfigParams {
            block_count: Some(64),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_count, 64);
        assert_eq!(config.cache_size, 512); // preserved
    }

    #[test]
    fn overrides_explicit_cache_size_always_wins() {
        let base = test_base_config();
        let cli = ImageConfigParams {
            read_size: Some(512),   // would trigger recomputation
            cache_size: Some(1024), // but explicit wins
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.cache_size, 1024);
    }

    // -------------------------------------------------------------------------
    // apply_cli_overrides: lookahead_size invalidation
    // -------------------------------------------------------------------------

    #[test]
    fn overrides_lookahead_recomputed_when_block_count_changes() {
        let base = test_base_config(); // block_count=128, lookahead=16
        let cli = ImageConfigParams {
            block_count: Some(64),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_count, 64);
        // lookahead_size recomputed by resolve(), not carried from base
        assert_ne!(config.lookahead_size, 0);
    }

    #[test]
    fn overrides_lookahead_recomputed_when_image_size_changes() {
        let base = test_base_config();
        let cli = ImageConfigParams {
            image_size: Some(4096 * 64),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_count, 64);
        // lookahead_size recomputed, not carried forward
        assert_ne!(config.lookahead_size, 0);
    }

    #[test]
    fn overrides_lookahead_preserved_when_block_count_unchanged() {
        let base = test_base_config(); // lookahead=16
        let cli = ImageConfigParams {
            block_cycles: Some(100), // unrelated change
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.lookahead_size, 16); // preserved
    }

    #[test]
    fn overrides_explicit_lookahead_always_wins() {
        let base = test_base_config();
        let cli = ImageConfigParams {
            block_count: Some(64),    // would trigger recomputation
            lookahead_size: Some(32), // but explicit wins
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.lookahead_size, 32);
    }

    // -------------------------------------------------------------------------
    // Flash helpers: needs_flash / mark_flashed
    // -------------------------------------------------------------------------

    #[test]
    fn needs_flash_returns_hash_for_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new_file_test.bin");
        fs::write(&file, b"hello").unwrap();

        let result = needs_flash(&file).unwrap();
        assert!(result.is_some(), "new file should need flashing");
    }

    #[test]
    fn needs_flash_returns_none_after_mark_flashed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mark_test.bin");
        fs::write(&file, b"hello").unwrap();

        let hash = needs_flash(&file).unwrap().unwrap();
        mark_flashed(&file, &hash).unwrap();

        let result = needs_flash(&file).unwrap();
        assert!(result.is_none(), "unchanged file should not need flashing");
    }

    #[test]
    fn needs_flash_returns_hash_after_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("changed_test.bin");
        fs::write(&file, b"hello").unwrap();

        let hash = needs_flash(&file).unwrap().unwrap();
        mark_flashed(&file, &hash).unwrap();

        // Modify the file
        fs::write(&file, b"world").unwrap();

        let result = needs_flash(&file).unwrap();
        assert!(result.is_some(), "modified file should need flashing");
    }

    #[test]
    fn needs_flash_errors_on_missing_file() {
        let result = needs_flash(Path::new("/nonexistent/file.bin"));
        assert!(result.is_err());
    }

    // -------------------------------------------------------------------------
    // run_command: template expansion and execution
    // -------------------------------------------------------------------------

    #[test]
    fn run_command_expands_placeholders() {
        // `true` ignores arguments and always succeeds
        run_command(
            "true {path} {address}",
            &[("path", "/fw.bin"), ("address", "0x1000")],
        )
        .unwrap();
    }

    #[test]
    fn run_command_no_placeholders() {
        run_command("true", &[]).unwrap();
    }

    #[test]
    fn run_command_nonzero_exit_is_error() {
        let result = run_command("false", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn run_command_missing_program_is_error() {
        let result = run_command("nonexistent_binary_that_should_not_exist_xxyz", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn run_command_empty_template_is_error() {
        let result = run_command("", &[]);
        assert!(result.is_err());
    }
}
