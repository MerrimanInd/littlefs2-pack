//! Cross-compatibility integration tests between the C++ `mklittlefs` and our
//! Rust implementation.
//!
//! Tests that require the C++ tool will be **skipped** (not failed) if
//! `mklittlefs` is not found on `$PATH`. Override the path by setting the
//! `MKLITTLEFS_CPP` environment variable.
//!
//! The Rust binary is obtained via `env!("CARGO_BIN_EXE_mklittlefs-rs")` which
//! cargo populates automatically for integration tests.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── Helpers ──────────────────────────────────────────────────────────────

/// Filesystem geometry used for all tests.
const BLOCK_SIZE: u32 = 4096;
const PAGE_SIZE: u32 = 256;
const IMAGE_SIZE: u32 = 131072; // 128 KiB

/// Return the path to the Rust binary under test.
fn rs_bin() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo for [[bin]] targets in integration tests.
    PathBuf::from(env!("CARGO_BIN_EXE_mklittlefs-rs"))
}

/// Return the path to the C++ mklittlefs, or `None` if unavailable.
fn cpp_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MKLITTLEFS_CPP") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Some(path);
        }
    }
    // Try to find on PATH
    which("mklittlefs")
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

/// Create a deterministic test fixture directory.
fn create_fixture(dir: &Path) {
    if dir.exists() {
        fs::remove_dir_all(dir).unwrap();
    }
    fs::create_dir_all(dir).unwrap();

    // Plain text
    fs::write(dir.join("hello.txt"), b"Hello from cross-compat test!\n").unwrap();

    // Binary blob (deterministic, not random, so diffs are meaningful)
    let blob: Vec<u8> = (0u8..=255).cycle().take(2048).collect();
    fs::write(dir.join("blob.bin"), &blob).unwrap();

    // Empty file
    fs::write(dir.join("empty.dat"), b"").unwrap();

    // Nested dirs
    fs::create_dir_all(dir.join("sub/nested")).unwrap();
    fs::write(dir.join("sub/readme.md"), b"# Readme\n").unwrap();
    fs::write(dir.join("sub/nested/deep.txt"), b"deep content\n").unwrap();

    // Unaligned-size file
    fs::write(dir.join("ten.bin"), b"0123456789").unwrap();

    // Larger file
    let repeated: Vec<u8> = b"littlefs\n".repeat(600);
    fs::write(dir.join("repeated.txt"), &repeated).unwrap();
}

/// Recursively read a directory into a map of relative-path → contents.
fn read_tree(root: &Path) -> HashMap<String, Vec<u8>> {
    let mut map = HashMap::new();
    walk(root, root, &mut map);
    map
}

fn walk(root: &Path, dir: &Path, map: &mut HashMap<String, Vec<u8>>) {
    let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let ft = entry.file_type().unwrap();
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        if ft.is_dir() {
            // Insert a sentinel so we know the dir exists
            map.insert(format!("{}/", rel), vec![]);
            walk(root, &entry.path(), map);
        } else if ft.is_file() {
            let data = fs::read(entry.path()).unwrap();
            map.insert(rel, data);
        }
    }
}

/// Assert two directory trees have identical file contents.
fn assert_trees_match(expected: &Path, actual: &Path) {
    let a = read_tree(expected);
    let b = read_tree(actual);

    // Check files in expected exist in actual with same content
    for (path, data) in &a {
        if path.ends_with('/') {
            assert!(
                b.contains_key(path),
                "directory {path} missing in unpacked output"
            );
        } else {
            let actual_data = b
                .get(path)
                .unwrap_or_else(|| panic!("file {path} missing in unpacked output"));
            assert_eq!(
                data,
                actual_data,
                "file {path} content mismatch (expected {} bytes, got {} bytes)",
                data.len(),
                actual_data.len()
            );
        }
    }

    // Check nothing extra in actual
    for path in b.keys() {
        assert!(
            a.contains_key(path),
            "unexpected extra entry {path} in unpacked output"
        );
    }
}

// ── C++ mklittlefs wrappers ─────────────────────────────────────────────

fn cpp_pack(cpp: &Path, src_dir: &Path, image: &Path) {
    let status = Command::new(cpp)
        .args([
            "-c",
            &src_dir.to_string_lossy(),
            "-b",
            &BLOCK_SIZE.to_string(),
            "-p",
            &PAGE_SIZE.to_string(),
            "-s",
            &IMAGE_SIZE.to_string(),
            &image.to_string_lossy(),
        ])
        .status()
        .expect("failed to run C++ mklittlefs");
    assert!(status.success(), "C++ mklittlefs -c failed: {status}");
}

fn cpp_unpack(cpp: &Path, image: &Path, dest_dir: &Path) {
    fs::create_dir_all(dest_dir).unwrap();
    let status = Command::new(cpp)
        .args([
            "-u",
            &dest_dir.to_string_lossy(),
            "-b",
            &BLOCK_SIZE.to_string(),
            "-p",
            &PAGE_SIZE.to_string(),
            "-s",
            &IMAGE_SIZE.to_string(),
            &image.to_string_lossy(),
        ])
        .status()
        .expect("failed to run C++ mklittlefs");
    assert!(status.success(), "C++ mklittlefs -u failed: {status}");
}

fn cpp_list(cpp: &Path, image: &Path) -> String {
    let output = Command::new(cpp)
        .args([
            "-l",
            "-b",
            &BLOCK_SIZE.to_string(),
            "-p",
            &PAGE_SIZE.to_string(),
            "-s",
            &IMAGE_SIZE.to_string(),
            &image.to_string_lossy(),
        ])
        .output()
        .expect("failed to run C++ mklittlefs");
    assert!(output.status.success(), "C++ mklittlefs -l failed");
    String::from_utf8_lossy(&output.stdout).to_string()
}

// ── Rust mklittlefs-rs wrappers ─────────────────────────────────────────

fn rs_pack(src_dir: &Path, image: &Path) {
    let status = Command::new(rs_bin())
        .args([
            "pack",
            "-d",
            &src_dir.to_string_lossy(),
            "-o",
            &image.to_string_lossy(),
            "-b",
            &BLOCK_SIZE.to_string(),
            "-p",
            &PAGE_SIZE.to_string(),
            "-s",
            &IMAGE_SIZE.to_string(),
        ])
        .status()
        .expect("failed to run mklittlefs-rs");
    assert!(status.success(), "mklittlefs-rs pack failed: {status}");
}

fn rs_unpack(image: &Path, dest_dir: &Path) {
    let status = Command::new(rs_bin())
        .args([
            "unpack",
            "-i",
            &image.to_string_lossy(),
            "-d",
            &dest_dir.to_string_lossy(),
            "-b",
            &BLOCK_SIZE.to_string(),
            "-p",
            &PAGE_SIZE.to_string(),
        ])
        .status()
        .expect("failed to run mklittlefs-rs");
    assert!(status.success(), "mklittlefs-rs unpack failed: {status}");
}

fn rs_list(image: &Path) -> String {
    let output = Command::new(rs_bin())
        .args([
            "list",
            "-i",
            &image.to_string_lossy(),
            "-b",
            &BLOCK_SIZE.to_string(),
            "-p",
            &PAGE_SIZE.to_string(),
        ])
        .output()
        .expect("failed to run mklittlefs-rs");
    assert!(output.status.success(), "mklittlefs-rs list failed");
    String::from_utf8_lossy(&output.stdout).to_string()
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Rust pack → Rust unpack (always runs, no C++ dependency).
#[test]
fn rust_self_roundtrip() {
    let tmp = tempdir("rust_self");
    let fixture = tmp.join("fixture");
    let image = tmp.join("image.bin");
    let unpacked = tmp.join("unpacked");

    create_fixture(&fixture);
    rs_pack(&fixture, &image);
    rs_unpack(&image, &unpacked);
    assert_trees_match(&fixture, &unpacked);
}

/// Pack with C++ mklittlefs → unpack with Rust mklittlefs-rs.
#[test]
fn cpp_pack_rust_unpack() {
    let cpp = match cpp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIPPED: C++ mklittlefs not found (set MKLITTLEFS_CPP or add to PATH)");
            return;
        }
    };

    let tmp = tempdir("cpp_pack_rs_unpack");
    let fixture = tmp.join("fixture");
    let image = tmp.join("image.bin");
    let unpacked = tmp.join("unpacked");

    create_fixture(&fixture);
    cpp_pack(&cpp, &fixture, &image);
    rs_unpack(&image, &unpacked);
    assert_trees_match(&fixture, &unpacked);
}

/// Pack with Rust mklittlefs-rs → unpack with C++ mklittlefs.
#[test]
fn rust_pack_cpp_unpack() {
    let cpp = match cpp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIPPED: C++ mklittlefs not found (set MKLITTLEFS_CPP or add to PATH)");
            return;
        }
    };

    let tmp = tempdir("rs_pack_cpp_unpack");
    let fixture = tmp.join("fixture");
    let image = tmp.join("image.bin");
    let unpacked = tmp.join("unpacked");

    create_fixture(&fixture);
    rs_pack(&fixture, &image);
    cpp_unpack(&cpp, &image, &unpacked);
    assert_trees_match(&fixture, &unpacked);
}

/// Pack with C++ → list with Rust. Verify all filenames appear.
#[test]
fn cpp_pack_rust_list() {
    let cpp = match cpp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIPPED: C++ mklittlefs not found");
            return;
        }
    };

    let tmp = tempdir("cpp_pack_rs_list");
    let fixture = tmp.join("fixture");
    let image = tmp.join("image.bin");

    create_fixture(&fixture);
    cpp_pack(&cpp, &fixture, &image);
    let listing = rs_list(&image);

    for name in &[
        "hello.txt",
        "blob.bin",
        "empty.dat",
        "sub",
        "deep.txt",
        "readme.md",
        "ten.bin",
        "repeated.txt",
    ] {
        assert!(
            listing.contains(name),
            "'{name}' not found in Rust listing:\n{listing}"
        );
    }
}

/// Pack with Rust → list with C++. Verify all filenames appear.
#[test]
fn rust_pack_cpp_list() {
    let cpp = match cpp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIPPED: C++ mklittlefs not found");
            return;
        }
    };

    let tmp = tempdir("rs_pack_cpp_list");
    let fixture = tmp.join("fixture");
    let image = tmp.join("image.bin");

    create_fixture(&fixture);
    rs_pack(&fixture, &image);
    let listing = cpp_list(&cpp, &image);

    for name in &[
        "hello.txt",
        "blob.bin",
        "empty.dat",
        "sub",
        "deep.txt",
        "readme.md",
        "ten.bin",
        "repeated.txt",
    ] {
        assert!(
            listing.contains(name),
            "'{name}' not found in C++ listing:\n{listing}"
        );
    }
}

/// Round-trip with smaller block size (256 bytes).
#[test]
fn small_block_roundtrip() {
    let cpp = match cpp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIPPED: C++ mklittlefs not found");
            return;
        }
    };

    let bs = 256u32;
    let ps = 16u32;
    let sz = 65536u32; // 256 blocks

    let tmp = tempdir("small_blocks");
    let fixture = tmp.join("fixture");
    fs::create_dir_all(&fixture).unwrap();
    fs::write(fixture.join("test.txt"), b"small blocks!\n").unwrap();
    fs::create_dir_all(fixture.join("d")).unwrap();
    fs::write(fixture.join("d/inner.txt"), b"inner\n").unwrap();

    // C++ → Rust
    {
        let image = tmp.join("small_cpp.bin");
        let unpacked = tmp.join("small_rs_out");

        let st = Command::new(&cpp)
            .args([
                "-c",
                &fixture.to_string_lossy(),
                "-b",
                &bs.to_string(),
                "-p",
                &ps.to_string(),
                "-s",
                &sz.to_string(),
                &image.to_string_lossy(),
            ])
            .status()
            .unwrap();
        assert!(st.success());

        let st = Command::new(rs_bin())
            .args([
                "unpack",
                "-i",
                &image.to_string_lossy(),
                "-d",
                &unpacked.to_string_lossy(),
                "-b",
                &bs.to_string(),
                "-p",
                &ps.to_string(),
            ])
            .status()
            .unwrap();
        assert!(st.success());

        assert_trees_match(&fixture, &unpacked);
    }

    // Rust → C++
    {
        let image = tmp.join("small_rs.bin");
        let unpacked = tmp.join("small_cpp_out");

        let st = Command::new(rs_bin())
            .args([
                "pack",
                "-d",
                &fixture.to_string_lossy(),
                "-o",
                &image.to_string_lossy(),
                "-b",
                &bs.to_string(),
                "-p",
                &ps.to_string(),
                "-s",
                &sz.to_string(),
            ])
            .status()
            .unwrap();
        assert!(st.success());

        fs::create_dir_all(&unpacked).unwrap();
        let st = Command::new(&cpp)
            .args([
                "-u",
                &unpacked.to_string_lossy(),
                "-b",
                &bs.to_string(),
                "-p",
                &ps.to_string(),
                "-s",
                &sz.to_string(),
                &image.to_string_lossy(),
            ])
            .status()
            .unwrap();
        assert!(st.success());

        assert_trees_match(&fixture, &unpacked);
    }
}

/// Full round-trip: Rust → C++ → Rust. Pack with Rust, unpack with C++,
/// re-pack with C++, unpack with Rust. Final output should match original.
#[test]
fn full_roundtrip_rs_cpp_rs() {
    let cpp = match cpp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIPPED: C++ mklittlefs not found");
            return;
        }
    };

    let tmp = tempdir("full_roundtrip");
    let fixture = tmp.join("fixture");
    let img1 = tmp.join("img1.bin");
    let mid = tmp.join("mid");
    let img2 = tmp.join("img2.bin");
    let final_out = tmp.join("final");

    create_fixture(&fixture);

    // Rust pack
    rs_pack(&fixture, &img1);

    // C++ unpack
    cpp_unpack(&cpp, &img1, &mid);

    // C++ re-pack
    cpp_pack(&cpp, &mid, &img2);

    // Rust unpack
    rs_unpack(&img2, &final_out);

    assert_trees_match(&fixture, &final_out);
}

/// Round-trip with a user-supplied fixture directory.
/// Skipped unless `TEST_FIXTURE_DIR` is set.
///
/// ```sh
/// TEST_FIXTURE_DIR=./my_fs_data cargo test --test cross_compat custom_fixture
/// ```
#[test]
fn custom_fixture_rust_roundtrip() {
    let fixture = match std::env::var("TEST_FIXTURE_DIR") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIPPED: TEST_FIXTURE_DIR not set");
            return;
        }
    };
    assert!(
        fixture.is_dir(),
        "TEST_FIXTURE_DIR={} is not a directory",
        fixture.display()
    );

    let tmp = tempdir("custom_rs");
    let image = tmp.join("custom.bin");
    let unpacked = tmp.join("unpacked");

    rs_pack(&fixture, &image);
    rs_unpack(&image, &unpacked);
    assert_trees_match(&fixture, &unpacked);
}

/// User-supplied fixture: Rust pack → C++ unpack.
/// Skipped unless both `TEST_FIXTURE_DIR` and the C++ tool are available.
#[test]
fn custom_fixture_rust_pack_cpp_unpack() {
    let fixture = match std::env::var("TEST_FIXTURE_DIR") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIPPED: TEST_FIXTURE_DIR not set");
            return;
        }
    };
    let cpp = match cpp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIPPED: C++ mklittlefs not found");
            return;
        }
    };
    assert!(fixture.is_dir());

    let tmp = tempdir("custom_rs_cpp");
    let image = tmp.join("custom.bin");
    let unpacked = tmp.join("unpacked");

    rs_pack(&fixture, &image);
    cpp_unpack(&cpp, &image, &unpacked);
    assert_trees_match(&fixture, &unpacked);
}

/// User-supplied fixture: C++ pack → Rust unpack.
/// Skipped unless both `TEST_FIXTURE_DIR` and the C++ tool are available.
#[test]
fn custom_fixture_cpp_pack_rust_unpack() {
    let fixture = match std::env::var("TEST_FIXTURE_DIR") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIPPED: TEST_FIXTURE_DIR not set");
            return;
        }
    };
    let cpp = match cpp_bin() {
        Some(p) => p,
        None => {
            eprintln!("SKIPPED: C++ mklittlefs not found");
            return;
        }
    };
    assert!(fixture.is_dir());

    let tmp = tempdir("custom_cpp_rs");
    let image = tmp.join("custom.bin");
    let unpacked = tmp.join("unpacked");

    cpp_pack(&cpp, &fixture, &image);
    rs_unpack(&image, &unpacked);
    assert_trees_match(&fixture, &unpacked);
}

// ── Temp dir helper ─────────────────────────────────────────────────────

fn tempdir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("mklittlefs_rs_test_{name}_{}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    dir
}
