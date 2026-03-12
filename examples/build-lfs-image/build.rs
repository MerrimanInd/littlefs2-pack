use std::path::Path;

use littlefs2_pack;

fn main() {
    littlefs2_pack::generate(&Path::new("./littlefs.toml"));
}
