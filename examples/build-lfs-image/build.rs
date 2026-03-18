use std::path::Path;

use littlefs2_pack;

fn main() {
    littlefs2_pack::pack_and_generate_config(&Path::new("./littlefs.toml"));
}
