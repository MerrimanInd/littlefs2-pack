use clap::{Args, Parser, Subcommand};
use littlefs2_pack::{LfsError, LfsImage, LfsImageConfig, MountedFs};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "littlefs",
    version,
    about = "Create, unpack, and inspect LittleFSv2 filesystem images"
)]
pub struct Cli {
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
/// These mirror the fields of the littlefs `lfs_config` struct.
/// Defaults match common ESP32 / RP2040 LittleFS setups.
#[derive(Args, Debug, Clone)]
pub struct FsParams {
    /// Filesystem block (erase unit) size in bytes.
    /// Must be >= 128 and a multiple of both read-size and write-size.
    /// Power-of-two values recommended (e.g. 256, 4096).
    #[arg(short, long, default_value_t = 4096)]
    pub block_size: u32,

    /// Total number of blocks in the filesystem.
    /// Mutually exclusive with --image-size; one of the two is required for `pack`.
    #[arg(short = 'c', long, conflicts_with = "image_size")]
    pub block_count: Option<u32>,

    /// Total image size in bytes (alternative to --block-count).
    /// Must be an exact multiple of --block-size.
    #[arg(short = 's', long, conflicts_with = "block_count")]
    pub image_size: Option<u32>,

    /// Minimum read size / page size in bytes.
    /// If only --page-size is given it sets both read and write size.
    #[arg(short, long, default_value_t = 256)]
    pub page_size: u32,

    /// Minimum read size in bytes (overrides --page-size for reads).
    #[arg(long)]
    pub read_size: Option<u32>,

    /// Minimum program (write) size in bytes (overrides --page-size for writes).
    #[arg(long)]
    pub write_size: Option<u32>,

    /// Block-cycle count for wear leveling.
    /// Higher values are more performant but less wear-leveled.
    /// -1 (the default) disables wear leveling entirely, which is fine for
    /// one-shot image creation.
    #[arg(long, default_value_t = -1, allow_hyphen_values = true)]
    pub block_cycles: i32,
}

impl FsParams {
    /// Effective read size.
    fn read_size(&self) -> u32 {
        self.read_size.unwrap_or(self.page_size)
    }

    /// Effective write size.
    fn write_size(&self) -> u32 {
        self.write_size.unwrap_or(self.page_size)
    }

    /// Resolve block_count, requiring it for operations that create images.
    fn resolve_block_count(&self, block_size: u32) -> Result<u32, String> {
        match (self.block_count, self.image_size) {
            (Some(bc), _) => Ok(bc),
            (None, Some(size)) => {
                if size % block_size != 0 {
                    Err(format!(
                        "image size ({size}) must be a multiple of block size ({block_size})"
                    ))
                } else {
                    Ok(size / block_size)
                }
            }
            (None, None) => {
                Err("either --block-count (-c) or --image-size (-s) must be specified".into())
            }
        }
    }

    /// Build an `LfsImageConfig`, resolving block count.
    fn to_config_with_count(&self) -> Result<LfsImageConfig, String> {
        let bc = self.resolve_block_count(self.block_size)?;
        Ok(LfsImageConfig {
            block_size: self.block_size,
            block_count: bc,
            read_size: self.read_size(),
            write_size: self.write_size(),
        })
    }

    /// Build an `LfsImageConfig` from an existing image's byte length,
    /// ignoring --block-count / --image-size.
    fn to_config_from_data_len(&self, data_len: usize) -> Result<LfsImageConfig, String> {
        let bs = self.block_size as usize;
        if data_len == 0 || data_len % bs != 0 {
            return Err(format!(
                "image file size ({data_len}) is not a multiple of block size ({bs})"
            ));
        }
        Ok(LfsImageConfig {
            block_size: self.block_size,
            block_count: (data_len / bs) as u32,
            read_size: self.read_size(),
            write_size: self.write_size(),
        })
    }
}

// ---------------------------------------------------------------------------
// Subcommand argument structs
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct Pack {
    /// Source directory to pack
    #[arg(short = 'd', long)]
    pub pack_directory: PathBuf,

    /// Output image file path
    #[arg(short, long)]
    pub output: PathBuf,

    #[command(flatten)]
    pub fs: FsParams,
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
    pub fs: FsParams,
}

#[derive(Args)]
pub struct ListCmd {
    /// LittleFS2 image file to inspect
    #[arg(short, long)]
    pub image: PathBuf,

    #[command(flatten)]
    pub fs: FsParams,
}

#[derive(Args)]
pub struct InfoCmd {
    /// LittleFS2 image file to inspect
    #[arg(short, long)]
    pub image: PathBuf,

    #[command(flatten)]
    pub fs: FsParams,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Pack(args) => cmd_pack(args)?,
        Commands::Unpack(args) => cmd_unpack(args)?,
        Commands::List(args) => cmd_list(args)?,
        Commands::Info(args) => cmd_info(args)?,
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// pack
// ---------------------------------------------------------------------------

fn cmd_pack(args: Pack) -> Result<(), Box<dyn std::error::Error>> {
    let config = args.fs.to_config_with_count()?;
    let block_count = config.block_count;
    let block_size = config.block_size;

    let mut image = LfsImage::new(config)?;
    image.format()?;

    image.mount_and_then(|fs| pack_directory(fs, &args.pack_directory, ""))?;

    let data = image.into_data();
    std::fs::write(&args.output, &data)?;

    println!(
        "Packed '{}' -> '{}' ({} bytes, {} blocks x {} bytes)",
        args.pack_directory.display(),
        args.output.display(),
        data.len(),
        block_count,
        block_size,
    );

    Ok(())
}

fn pack_directory(
    fs: &MountedFs<'_>,
    host_dir: &std::path::Path,
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
            pack_directory(fs, &entry.path(), &lfs_path)?;
        } else if file_type.is_file() {
            let data = std::fs::read(entry.path()).map_err(|e| LfsError::Io(e.to_string()))?;
            println!("  write  {lfs_path} ({} bytes)", data.len());
            fs.write_file(&lfs_path, &data)?;
        }
        // Skip symlinks and other special files
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// unpack
// ---------------------------------------------------------------------------

fn cmd_unpack(args: Unpack) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(&args.image)?;
    let config = args.fs.to_config_from_data_len(data.len())?;
    let mut image = LfsImage::from_data(config, data)?;

    std::fs::create_dir_all(&args.unpack_directory)?;

    image.mount_and_then(|fs| unpack_directory(fs, "/", &args.unpack_directory))?;

    println!(
        "Unpacked '{}' -> '{}'",
        args.image.display(),
        args.unpack_directory.display()
    );

    Ok(())
}

fn unpack_directory(
    fs: &MountedFs<'_>,
    lfs_dir: &str,
    host_dir: &std::path::Path,
) -> Result<(), LfsError> {
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

fn cmd_list(args: ListCmd) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(&args.image)?;
    let config = args.fs.to_config_from_data_len(data.len())?;
    let mut image = LfsImage::from_data(config, data)?;

    image.mount_and_then(|fs| list_directory(fs, "/", 0))?;

    Ok(())
}

fn list_directory(fs: &MountedFs<'_>, lfs_dir: &str, depth: usize) -> Result<(), LfsError> {
    let entries = fs.read_dir(lfs_dir)?;
    let indent = "  ".repeat(depth);

    for entry in entries {
        if entry.is_dir {
            println!("{indent}{}/ (dir)", entry.name);
            let sub = if lfs_dir == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{lfs_dir}/{}", entry.name)
            };
            list_directory(fs, &sub, depth + 1)?;
        } else {
            println!("{indent}{} ({} bytes)", entry.name, entry.size);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

fn cmd_info(args: InfoCmd) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(&args.image)?;
    let config = args.fs.to_config_from_data_len(data.len())?;
    let bc = config.block_count;
    let bs = config.block_size;
    let mut image = LfsImage::from_data(config, data)?;

    image.mount_and_then(|fs| {
        let used = fs.used_blocks()?;
        let total = bc as usize;
        let free = total.saturating_sub(used);

        println!("Image size:   {} bytes", total * bs as usize);
        println!("Block size:   {} bytes", bs);
        println!("Block count:  {}", total);
        println!("Blocks used:  {} ({} bytes)", used, used * bs as usize);
        println!("Blocks free:  {} ({} bytes)", free, free * bs as usize);
        Ok(())
    })?;

    Ok(())
}
