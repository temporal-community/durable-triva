#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
generated_defaults="$project_dir/.sdkconfig.partition.defaults"
next_defaults="$generated_defaults.next"
build_metadata_dir="$project_dir/.firmware-build-metadata"
version_file="$build_metadata_dir/version.txt"
next_version_file="$version_file.next"
version_tracking_stamp="$build_metadata_dir/native-version-tracking-v2"

if [ -n "${ESP_GCC_DIR:-}" ]; then
    PATH="$ESP_GCC_DIR:$PATH"
fi
if ! command -v xtensa-esp32s3-elf-gcc >/dev/null 2>&1; then
    tools_root=${IDF_TOOLS_PATH:-$HOME/.espressif}
    for candidate in "$tools_root"/tools/xtensa-esp-elf/*/xtensa-esp-elf/bin; do
        if [ -x "$candidate/xtensa-esp32s3-elf-gcc" ]; then
            PATH="$candidate:$PATH"
            break
        fi
    done
fi
if ! command -v xtensa-esp32s3-elf-gcc >/dev/null 2>&1; then
    echo "missing xtensa-esp32s3-elf-gcc; run espup install and source its export file" >&2
    exit 1
fi
if [ -x "$project_dir/.tools/bin/ldproxy" ]; then
    PATH="$project_dir/.tools/bin:$PATH"
fi
if ! command -v ldproxy >/dev/null 2>&1; then
    echo "missing ldproxy; install it with: cargo install ldproxy" >&2
    exit 1
fi
export PATH

mkdir -p "$build_metadata_dir"
firmware_version=$(git -c core.fsmonitor=false describe --always --dirty)
printf '%s\n' "$firmware_version" > "$next_version_file"
if ! cmp -s "$next_version_file" "$version_file"; then
    mv "$next_version_file" "$version_file"
else
    rm "$next_version_file"
fi

# Existing Cargo caches predate the tracked version file, so bootstrap them once.
# After this clean build, esp-idf-sys watches version.txt and rebuilds only when
# the Git description changes.
if [ ! -f "$version_tracking_stamp" ]; then
    (cd "$project_dir" && cargo clean -p esp-idf-sys \
        --target xtensa-esp32s3-espidf --release)
    touch "$version_tracking_stamp"
fi

printf 'CONFIG_PARTITION_TABLE_CUSTOM_FILENAME="%s/firmware/partitions.csv"\n' \
    "$project_dir" > "$next_defaults"
if ! cmp -s "$next_defaults" "$generated_defaults"; then
    mv "$next_defaults" "$generated_defaults"
else
    rm "$next_defaults"
fi

export ESP_IDF_SDKCONFIG_DEFAULTS="$project_dir/firmware/sdkconfig.defaults;$generated_defaults"
export ESP_IDF_SYS_ROOT_CRATE="temporal-trivia-badge-firmware"
export ESP_IDF_GLOB_BUILD_METADATA_BASE="$build_metadata_dir"
export ESP_IDF_GLOB_BUILD_METADATA_VERSION="version.txt"
export BADGE_BUILD_UNIX_EPOCH=$(date +%s)

cd "$project_dir"
cargo build -j 2 -p temporal-trivia-badge-firmware --release "$@"

firmware_elf="$project_dir/target/xtensa-esp32s3-espidf/release/temporal-trivia-badge-firmware"
if ! strings "$firmware_elf" | grep -Fqx "$firmware_version"; then
    echo "firmware metadata mismatch: expected embedded version $firmware_version" >&2
    exit 1
fi
echo "verified embedded firmware version: $firmware_version"

# The HIL reader can inject a correct answer into a live round, so the image
# people carry must not contain it. Assert the gate rather than trusting it.
case " $* " in
    *" hil"*) hil_expected=1 ;;
    *) hil_expected=0 ;;
esac
if strings "$firmware_elf" | grep -Fq "HIL ACK answer="; then
    hil_embedded=1
else
    hil_embedded=0
fi
if [ "$hil_embedded" != "$hil_expected" ]; then
    echo "firmware HIL gating mismatch: expected hil=$hil_expected, image has hil=$hil_embedded" >&2
    exit 1
fi
if [ "$hil_embedded" = 1 ]; then
    echo "verified embedded HIL test protocol: acceptance build, do not hand this badge out"
else
    echo "verified no HIL test protocol in the image"
fi
