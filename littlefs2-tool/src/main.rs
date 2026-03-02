use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use littlefs2_config::{Config, ImageConfig};
use littlefs2_pack::{LfsError, LfsImage, MountedFs};
use littlefs2_tool::pack::pack_directory;
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
    Pack(Pack),
    /// Unpack a LittleFS2 image into a directory
    Unpack(Unpack),
    /// List files in a LittleFS2 image
    List(ListCmd),
    /// Print info about a LittleFS2 image (block count, used space, etc.)
    Info(InfoCmd),
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

    let mut builder = ImageConfig::new()
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

    Ok(builder.validated()?)
}

/// Apply CLI overrides to an `ImageConfig` loaded from TOML.
///
/// Starts from the TOML values, then overwrites anything the user
/// explicitly passed on the command line.
fn apply_cli_overrides(base: &ImageConfig, cli: &ImageConfigParams) -> ImageConfig {
    let mut builder = ImageConfig::new()
        .with_block_size(cli.block_size.unwrap_or(base.block_size()))
        .with_read_size(cli.read_size.unwrap_or(base.read_size()))
        .with_write_size(cli.write_size.unwrap_or(base.write_size()))
        .with_block_cycles(cli.block_cycles.unwrap_or(base.block_cycles()));

    // If the user passed --image-size, use that instead of the TOML's block_count
    if let Some(s) = cli.image_size {
        builder = builder.with_image_size(s);
    } else {
        builder = builder.with_block_count(cli.block_count.unwrap_or(base.block_count()));
    }

    builder
        .validated()
        .expect("TOML config was valid, overrides should not invalidate it")
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
    let (block_size, read_size, write_size, block_cycles) = match config_path {
        Some(path) => {
            let config = Config::from_file(path)?;
            (
                cli.block_size.unwrap_or(config.image.block_size()),
                cli.read_size.unwrap_or(config.image.read_size()),
                cli.write_size.unwrap_or(config.image.write_size()),
                cli.block_cycles.unwrap_or(config.image.block_cycles()),
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
            )
        }
    };

    if data.is_empty() || data.len() % block_size != 0 {
        bail!(
            "image file size ({}) is not a multiple of block_size ({block_size})",
            data.len()
        );
    }

    Ok(ImageConfig::new()
        .with_block_size(block_size)
        .with_block_count(data.len() / block_size)
        .with_read_size(read_size)
        .with_write_size(write_size)
        .with_block_cycles(block_cycles)
        .validated()?)
}

// ---------------------------------------------------------------------------
// Subcommand argument structs
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct Pack {
    /// Source directory to pack (overrides TOML [directory] root)
    #[arg(short = 'd', long)]
    pub pack_directory: Option<PathBuf>,

    /// Output image file path
    #[arg(short, long)]
    pub output: PathBuf,

    #[command(flatten)]
    pub fs: ImageConfigParams,
}

#[derive(Args)]
pub struct Unpack {
    /// LittleFS2 image file to unpack
    #[arg(short, long)]
    pub image: PathBuf,

    /// Output directory
    #[arg(short = 'd', long)]
    pub unpack_directory: PathBuf,

    #[command(flatten)]
    pub fs: ImageConfigParams,
}

#[derive(Args)]
pub struct ListCmd {
    /// LittleFS2 image file to inspect
    #[arg(short, long)]
    pub image: PathBuf,

    #[command(flatten)]
    pub fs: ImageConfigParams,
}

#[derive(Args)]
pub struct InfoCmd {
    /// LittleFS2 image file to inspect
    #[arg(short, long)]
    pub image: PathBuf,

    #[command(flatten)]
    pub fs: ImageConfigParams,
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
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// pack
// ---------------------------------------------------------------------------

fn cmd_pack(config_path: &Option<PathBuf>, args: Pack) -> Result<()> {
    // Resolve everything from TOML + CLI overrides
    let (image_config, root, directory_config) = match config_path {
        Some(path) => {
            let config = Config::from_file(path)?;
            let image_config = apply_cli_overrides(&config.image, &args.fs);
            let root = args
                .pack_directory
                .unwrap_or_else(|| config.base_dir().join(config.directory.root()));
            (image_config, root, Some(config.directory))
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

    let block_count = image_config.block_count();
    let block_size = image_config.block_size();

    let mut image = LfsImage::new(image_config)?;
    image.format()?;

    image.mount_and_then(|fs| match &directory_config {
        Some(dir_config) => {
            pack_directory(fs, dir_config, &root).map_err(|e| LfsError::Io(e.to_string()))
        }
        None => pack_directory_simple(fs, &root, ""),
    })?;

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

/// Simple recursive directory packing without ignore/glob rules.
/// Used when no TOML config is provided.
fn pack_directory_simple(
    fs: &MountedFs<'_>,
    host_dir: &Path,
    lfs_prefix: &str,
) -> Result<(), LfsError> {
    let mut entries: Vec<_> = std::fs::read_dir(host_dir)
        .map_err(|e| LfsError::Io(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| LfsError::Io(e.to_string()))?;

    // Sort for deterministic output
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let file_type = entry.file_type().map_err(|e| LfsError::Io(e.to_string()))?;
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
            let data = std::fs::read(entry.path()).map_err(|e| LfsError::Io(e.to_string()))?;
            println!("  write  {lfs_path} ({} bytes)", data.len());
            fs.write_file(&lfs_path, &data)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// unpack
// ---------------------------------------------------------------------------

fn cmd_unpack(config_path: &Option<PathBuf>, args: Unpack) -> Result<()> {
    let data = std::fs::read(&args.image)
        .with_context(|| format!("failed to read image '{}'", args.image.display()))?;
    let config = image_config_for_reading(config_path, &args.fs, &data)?;
    let mut image = LfsImage::from_data(config, data)?;

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
            std::fs::create_dir_all(&host_path).map_err(|e| LfsError::Io(e.to_string()))?;
            unpack_directory(fs, &lfs_child, &host_path)?;
        } else {
            let data = fs.read_file(&lfs_child)?;
            std::fs::write(&host_path, &data).map_err(|e| LfsError::Io(e.to_string()))?;
            println!("  extract {} ({} bytes)", host_path.display(), data.len());
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn cmd_list(config_path: &Option<PathBuf>, args: ListCmd) -> Result<()> {
    let data = std::fs::read(&args.image)
        .with_context(|| format!("failed to read image '{}'", args.image.display()))?;
    let config = image_config_for_reading(config_path, &args.fs, &data)?;
    let mut image = LfsImage::from_data(config, data)?;

    image.mount_and_then(|fs| {
        println!("/");
        list_directory(fs, "/", "")
    })?;

    Ok(())
}

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

fn cmd_info(config_path: &Option<PathBuf>, args: InfoCmd) -> Result<()> {
    let data = std::fs::read(&args.image)
        .with_context(|| format!("failed to read image '{}'", args.image.display()))?;
    let config = image_config_for_reading(config_path, &args.fs, &data)?;

    let bc = config.block_count();
    let bs = config.block_size();

    let mut image = LfsImage::from_data(config, data)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use littlefs2_config::ImageConfig;
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
        assert_eq!(config.block_size(), 4096);
        assert_eq!(config.block_count(), 64);
        assert_eq!(config.read_size(), 256);
        assert_eq!(config.write_size(), 256);
        assert_eq!(config.block_cycles(), -1);
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
        assert_eq!(config.block_count(), 64);
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
        assert_eq!(config.read_size(), 16);
        assert_eq!(config.write_size(), 512);
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
        assert_eq!(config.read_size(), 16);
        assert_eq!(config.write_size(), 512);
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
        assert_eq!(config.block_cycles(), 500);
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
        assert_eq!(config.block_cycles(), -1);
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

    #[test]
    fn overrides_no_cli_args_preserves_toml() {
        let base = ImageConfig::from(4096, 128, 16, 512);
        let cli = empty_cli();

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_size(), 4096);
        assert_eq!(config.block_count(), 128);
        assert_eq!(config.read_size(), 16);
        assert_eq!(config.write_size(), 512);
    }

    #[test]
    fn overrides_block_size() {
        let base = ImageConfig::from(4096, 128, 256, 256);
        let cli = ImageConfigParams {
            block_size: Some(512),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_size(), 512);
        assert_eq!(config.block_count(), 128); // preserved from TOML
    }

    #[test]
    fn overrides_block_count() {
        let base = ImageConfig::from(4096, 128, 256, 256);
        let cli = ImageConfigParams {
            block_count: Some(64),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_count(), 64);
    }

    #[test]
    fn overrides_image_size_replaces_block_count() {
        let base = ImageConfig::from(4096, 128, 256, 256);
        let cli = ImageConfigParams {
            image_size: Some(4096 * 32),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_count(), 32);
        assert_eq!(config.image_size(), 4096 * 32);
    }

    #[test]
    fn overrides_read_write_sizes() {
        let base = ImageConfig::from(4096, 128, 256, 256);
        let cli = ImageConfigParams {
            read_size: Some(16),
            write_size: Some(512),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.read_size(), 16);
        assert_eq!(config.write_size(), 512);
    }

    #[test]
    fn overrides_block_cycles() {
        let base = ImageConfig::from(4096, 128, 256, 256);
        let cli = ImageConfigParams {
            block_cycles: Some(100),
            ..empty_cli()
        };

        let config = apply_cli_overrides(&base, &cli);
        assert_eq!(config.block_cycles(), 100);
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
        assert_eq!(config.block_count(), 64);
        // Other params come from TOML
        assert_eq!(config.block_size(), 4096);
        assert_eq!(config.read_size(), 16);
        assert_eq!(config.write_size(), 512);
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

        assert_eq!(config.read_size(), 32);
        assert_eq!(config.write_size(), 512); // from TOML
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

        assert_eq!(config.block_size(), 4096);
        assert_eq!(config.block_count(), 32);
        assert_eq!(config.read_size(), 256);
        assert_eq!(config.write_size(), 256);
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
}
