#!/usr/bin/env bash
#
# Cross-compatibility integration tests between the C++ mklittlefs and our
# Rust mklittlefs-rs.
#
# Verifies that images created by one tool can be correctly read by the other,
# ensuring on-disk format compatibility.
#
# Requirements:
#   - mklittlefs   (C++ version) on $PATH, or set $MKLITTLEFS_CPP
#   - mklittlefs-rs (Rust version) on $PATH, or set $MKLITTLEFS_RS
#
# Usage:
#   ./tests/cross_compat.sh
#   MKLITTLEFS_CPP=/path/to/mklittlefs MKLITTLEFS_RS=./target/debug/mklittlefs-rs ./tests/cross_compat.sh

set -euo pipefail

# ---------------------------------------------------------------------------
# Resolve tool paths
# ---------------------------------------------------------------------------

CPP="${MKLITTLEFS_CPP:-mklittlefs}"
RS="${MKLITTLEFS_RS:-mklittlefs-rs}"

# Verify both tools are available
if ! command -v "$CPP" &>/dev/null; then
    echo "ERROR: C++ mklittlefs not found. Set MKLITTLEFS_CPP or put it on PATH."
    exit 1
fi
if ! command -v "$RS" &>/dev/null; then
    echo "ERROR: Rust mklittlefs-rs not found. Set MKLITTLEFS_RS or put it on PATH."
    exit 1
fi

echo "C++ tool:  $CPP  ($("$CPP" --version 2>&1 | head -1 || true))"
echo "Rust tool: $RS"
echo ""

# ---------------------------------------------------------------------------
# Test parameters — use values that both tools agree on
# ---------------------------------------------------------------------------

BLOCK_SIZE=4096
PAGE_SIZE=256
IMAGE_SIZE=131072  # 128 KiB = 32 blocks of 4096
BLOCK_COUNT=$((IMAGE_SIZE / BLOCK_SIZE))

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

PASS=0
FAIL=0

pass() {
    PASS=$((PASS + 1))
    echo "  PASS: $1"
}

fail() {
    FAIL=$((FAIL + 1))
    echo "  FAIL: $1"
}

# Create a reproducible test fixture directory with various file types/sizes.
create_test_fixture() {
    local dir="$1"
    rm -rf "$dir"
    mkdir -p "$dir"

    # Plain text file
    echo "Hello from the cross-compatibility test!" > "$dir/hello.txt"

    # Binary-ish file (sequence of bytes)
    dd if=/dev/urandom of="$dir/random.bin" bs=1024 count=2 2>/dev/null

    # Empty file
    touch "$dir/empty.dat"

    # Nested directories
    mkdir -p "$dir/subdir/nested"
    echo "I am nested" > "$dir/subdir/nested/deep.txt"
    echo "Top of subdir" > "$dir/subdir/readme.md"

    # A file whose size is not aligned to page boundaries
    printf 'ABCDEFGHIJ' > "$dir/ten_bytes.bin"

    # A slightly larger file
    yes "littlefs" | head -c 5000 > "$dir/repeated.txt"
}

# Compare two directories recursively.
# Returns 0 if identical, 1 otherwise.
dirs_match() {
    local a="$1"
    local b="$2"

    # diff -r compares directory trees recursively
    if diff -r "$a" "$b" > /dev/null 2>&1; then
        return 0
    else
        return 1
    fi
}

# ---------------------------------------------------------------------------
# Test 1: Pack with C++ → Unpack with Rust
# ---------------------------------------------------------------------------

test_cpp_pack_rust_unpack() {
    local test_name="C++ pack → Rust unpack"
    echo ""
    echo "=== $test_name ==="

    local fixture="$TMPDIR/fixture1"
    local image="$TMPDIR/cpp_packed.img"
    local unpacked="$TMPDIR/rust_unpacked"

    create_test_fixture "$fixture"

    # C++ packs:  mklittlefs -c <dir> -b <block> -p <page> -s <size> <image>
    echo "  Packing with C++ tool..."
    "$CPP" -c "$fixture" -b "$BLOCK_SIZE" -p "$PAGE_SIZE" -s "$IMAGE_SIZE" "$image"

    if [ ! -f "$image" ]; then
        fail "$test_name — C++ tool did not produce an image"
        return
    fi

    # Rust unpacks
    echo "  Unpacking with Rust tool..."
    rm -rf "$unpacked"
    "$RS" unpack -i "$image" -d "$unpacked" -b "$BLOCK_SIZE" -p "$PAGE_SIZE"

    if [ ! -d "$unpacked" ]; then
        fail "$test_name — Rust tool did not produce output directory"
        return
    fi

    # Compare
    if dirs_match "$fixture" "$unpacked"; then
        pass "$test_name"
    else
        fail "$test_name — directory contents differ"
        echo "    --- diff ---"
        diff -r "$fixture" "$unpacked" || true
    fi
}

# ---------------------------------------------------------------------------
# Test 2: Pack with Rust → Unpack with C++
# ---------------------------------------------------------------------------

test_rust_pack_cpp_unpack() {
    local test_name="Rust pack → C++ unpack"
    echo ""
    echo "=== $test_name ==="

    local fixture="$TMPDIR/fixture2"
    local image="$TMPDIR/rust_packed.img"
    local unpacked="$TMPDIR/cpp_unpacked"

    create_test_fixture "$fixture"

    # Rust packs
    echo "  Packing with Rust tool..."
    "$RS" pack -d "$fixture" -o "$image" -b "$BLOCK_SIZE" -p "$PAGE_SIZE" \
        -s "$IMAGE_SIZE"

    if [ ! -f "$image" ]; then
        fail "$test_name — Rust tool did not produce an image"
        return
    fi

    # C++ unpacks:  mklittlefs -u <dir> -b <block> -p <page> -s <size> <image>
    echo "  Unpacking with C++ tool..."
    rm -rf "$unpacked"
    mkdir -p "$unpacked"
    "$CPP" -u "$unpacked" -b "$BLOCK_SIZE" -p "$PAGE_SIZE" -s "$IMAGE_SIZE" "$image"

    # Compare
    if dirs_match "$fixture" "$unpacked"; then
        pass "$test_name"
    else
        fail "$test_name — directory contents differ"
        echo "    --- diff ---"
        diff -r "$fixture" "$unpacked" || true
    fi
}

# ---------------------------------------------------------------------------
# Test 3: Pack with C++ → List with Rust  (smoke test)
# ---------------------------------------------------------------------------

test_cpp_pack_rust_list() {
    local test_name="C++ pack → Rust list"
    echo ""
    echo "=== $test_name ==="

    local fixture="$TMPDIR/fixture3"
    local image="$TMPDIR/cpp_list.img"

    create_test_fixture "$fixture"

    "$CPP" -c "$fixture" -b "$BLOCK_SIZE" -p "$PAGE_SIZE" -s "$IMAGE_SIZE" "$image"

    echo "  Listing with Rust tool..."
    local listing
    listing=$("$RS" list -i "$image" -b "$BLOCK_SIZE" -p "$PAGE_SIZE" 2>&1)

    # Check that key files appear in the listing
    local ok=true
    for name in hello.txt random.bin empty.dat subdir deep.txt readme.md ten_bytes.bin repeated.txt; do
        if echo "$listing" | grep -q "$name"; then
            :
        else
            echo "    missing from listing: $name"
            ok=false
        fi
    done

    if $ok; then
        pass "$test_name"
    else
        fail "$test_name — some files missing from listing"
        echo "    Full listing:"
        echo "$listing" | sed 's/^/      /'
    fi
}

# ---------------------------------------------------------------------------
# Test 4: Pack with Rust → List with C++ (smoke test)
# ---------------------------------------------------------------------------

test_rust_pack_cpp_list() {
    local test_name="Rust pack → C++ list"
    echo ""
    echo "=== $test_name ==="

    local fixture="$TMPDIR/fixture4"
    local image="$TMPDIR/rust_list.img"

    create_test_fixture "$fixture"

    "$RS" pack -d "$fixture" -o "$image" -b "$BLOCK_SIZE" -p "$PAGE_SIZE" \
        -s "$IMAGE_SIZE"

    echo "  Listing with C++ tool..."
    local listing
    listing=$("$CPP" -l -b "$BLOCK_SIZE" -p "$PAGE_SIZE" -s "$IMAGE_SIZE" "$image" 2>&1)

    local ok=true
    for name in hello.txt random.bin empty.dat subdir deep.txt readme.md ten_bytes.bin repeated.txt; do
        if echo "$listing" | grep -q "$name"; then
            :
        else
            echo "    missing from listing: $name"
            ok=false
        fi
    done

    if $ok; then
        pass "$test_name"
    else
        fail "$test_name — some files missing from listing"
        echo "    Full listing:"
        echo "$listing" | sed 's/^/      /'
    fi
}

# ---------------------------------------------------------------------------
# Test 5: Round-trip both directions with a small block size
# ---------------------------------------------------------------------------

test_small_blocks() {
    local test_name="Small block size (256) round-trip"
    echo ""
    echo "=== $test_name ==="

    local bs=256
    local sz=65536   # 256 blocks
    local ps=16

    local fixture="$TMPDIR/fixture_small"
    rm -rf "$fixture"
    mkdir -p "$fixture"
    echo "small block test" > "$fixture/test.txt"
    mkdir -p "$fixture/dir"
    echo "nested" > "$fixture/dir/nested.txt"

    # C++ → Rust
    local img_a="$TMPDIR/small_cpp.img"
    local out_a="$TMPDIR/small_rust_out"
    "$CPP" -c "$fixture" -b "$bs" -p "$ps" -s "$sz" "$img_a"
    rm -rf "$out_a"
    "$RS" unpack -i "$img_a" -d "$out_a" -b "$bs" -p "$ps"

    if dirs_match "$fixture" "$out_a"; then
        pass "$test_name (C++ → Rust)"
    else
        fail "$test_name (C++ → Rust)"
        diff -r "$fixture" "$out_a" || true
    fi

    # Rust → C++
    local img_b="$TMPDIR/small_rust.img"
    local out_b="$TMPDIR/small_cpp_out"
    "$RS" pack -d "$fixture" -o "$img_b" -b "$bs" -p "$ps" -s "$sz"
    rm -rf "$out_b"
    mkdir -p "$out_b"
    "$CPP" -u "$out_b" -b "$bs" -p "$ps" -s "$sz" "$img_b"

    if dirs_match "$fixture" "$out_b"; then
        pass "$test_name (Rust → C++)"
    else
        fail "$test_name (Rust → C++)"
        diff -r "$fixture" "$out_b" || true
    fi
}

# ---------------------------------------------------------------------------
# Test 6: Rust self round-trip (no C++ dependency, always runs)
# ---------------------------------------------------------------------------

test_rust_self_roundtrip() {
    local test_name="Rust self round-trip"
    echo ""
    echo "=== $test_name ==="

    local fixture="$TMPDIR/fixture_self"
    local image="$TMPDIR/self.img"
    local unpacked="$TMPDIR/self_unpacked"

    create_test_fixture "$fixture"

    "$RS" pack -d "$fixture" -o "$image" -b "$BLOCK_SIZE" -p "$PAGE_SIZE" \
        -s "$IMAGE_SIZE"
    rm -rf "$unpacked"
    "$RS" unpack -i "$image" -d "$unpacked" -b "$BLOCK_SIZE" -p "$PAGE_SIZE"

    if dirs_match "$fixture" "$unpacked"; then
        pass "$test_name"
    else
        fail "$test_name"
        diff -r "$fixture" "$unpacked" || true
    fi
}

# ---------------------------------------------------------------------------
# Run all tests
# ---------------------------------------------------------------------------

echo "========================================"
echo "  mklittlefs cross-compatibility tests"
echo "========================================"

test_rust_self_roundtrip
test_cpp_pack_rust_unpack
test_rust_pack_cpp_unpack
test_cpp_pack_rust_list
test_rust_pack_cpp_list
test_small_blocks

echo ""
echo "========================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "========================================"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
