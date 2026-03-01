use clap::{Args, Parser, Subcommand};
use lib;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "lfstool",
    version,
    about = "Create, unpack, and inspect LittleFS2 filesystem images"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Command for deploying an image to a microcontroller
    Deploy(Deploy),
    /// Commands related to the LittleFS2 image
    Image(Image),
}

#[derive(Subcommand)]
pub enum Image {
    /// Pack a directory into a LittleFS2 image
    #[command(subcommand)]
    Pack(Pack),
    /// Unpack a LittleFS2 image into a directory
    #[command(subcommand)]
    Unpack(Unpack),
    /// List files in a LittleFS2 image
    #[command(subcommand)]
    List(ListCmd),
    /// Print info about a LittleFS2 image (block count, used space, etc.)
    #[command(subcommand)]
    Info(InfoCmd),
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Deploy(_) => todo!(),
        Commands::Image(image) => match image {
            Image::Pack(_) => todo!(),
            Image::Unpack(_) => todo!(),
            Image::List(_) => todo!(),
            Image::Info(_) => todo!(),
        },
    }

    Ok(())
}
