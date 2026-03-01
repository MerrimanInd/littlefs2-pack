use std::path::PathBuf;

use ignore::WalkBuilder;
use littlefs2_pack::{LfsImage, LfsImageConfig};

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("Error packing the image: {0}")]
    Lfs(#[from] littlefs2_pack::LfsError),
    #[error("Error walking the directory: {0}")]
    Ignore(#[from] ignore::Error),
    #[error("Error reading a file: {0}")]
    Io(#[from] std::io::Error),
}

/// This enum sets the behavior when littlefs2-send
/// encounters a file that cannot be read or packed
pub enum OnError {
    Fail,
    Ignore,
    Log(PathBuf),
}

/// This struct represents a local directory that will be
/// packed and sent to the microcontroller on flashing.
struct ImageDirectory {
    builder: WalkBuilder,
    on_error: OnError,
    image: LfsImage,
    path: PathBuf,
}

impl ImageDirectory {
    /// Create a new image target directory that ignores
    /// hidden files and folders while respecting .gitignore
    fn new(config: LfsImageConfig, directory_root: PathBuf) -> Result<Self, ImageError> {
        Ok(Self {
            builder: WalkBuilder::new(directory_root.clone()),
            image: LfsImage::new(config)?,
            path: directory_root,
            on_error: OnError::Ignore,
        })
    }

    /// Create a new image target directory from an
    /// `ignore::WalkBuilder` struct.
    fn from_builder(
        config: LfsImageConfig,
        directory_root: PathBuf,
        builder: WalkBuilder,
    ) -> Result<Self, ImageError> {
        Ok(Self {
            builder: builder,
            image: LfsImage::new(config)?,
            path: directory_root,
            on_error: OnError::Ignore,
        })
    }

    /// Builder pattern method for setting the
    /// ignore_file_errors option
    fn ignore_file_errors(mut self) -> Self {
        self.on_error = OnError::Fail;
        return self;
    }

    /// Consume the ImageDirectory object and return
    /// its inner LfsImage for flashing to the micro.
    fn image(self) -> LfsImage {
        self.image
    }

    /// Walk the directory and pack the discovered contents into
    /// the LittleFS file image
    fn pack(&mut self) -> Result<(), ImageError> {
        let handle_err = |e: ImageError| -> Result<(), ImageError> {
            match &self.on_error {
                OnError::Fail => Err(e),
                OnError::Ignore => Ok(()),
                OnError::Log(path) => {
                    use std::io::Write;
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)?;
                    writeln!(file, "{e}")?;
                    Ok(())
                }
            }
        };

        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();

        for entry in self.builder.build() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    handle_err(e.into())?;
                    continue;
                }
            };

            if entry.depth() == 0 {
                continue;
            }

            let ft = entry.file_type().unwrap();
            if ft.is_dir() {
                dirs.push(entry.path().to_owned());
            } else if ft.is_file() {
                match std::fs::read(entry.path()) {
                    Ok(data) => files.push((entry.path().to_owned(), data)),
                    Err(e) => handle_err(e.into())?,
                }
            }
        }

        self.image.mount_and_then(|fs| {
            for path in &dirs {
                fs.create_dir(path.to_str().unwrap())?;
            }
            for (path, data) in &files {
                fs.write_file(path.to_str().unwrap(), data)?;
            }
            Ok(())
        })?;

        Ok(())
    }
}
