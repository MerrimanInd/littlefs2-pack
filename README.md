# LittleFS Tooling

This project provides a Rust CLI for working with [the LittleFS file system](https://github.com/littlefs-project/littlefs). It can pack a directory into a LittleFS image, unpack an image back into its directory structure, and inspect the contents of an image. It can also synchronize a local directory to a microcontroller by building the image and sending it to the micro (if the files have changed) as part of the flashing process.
