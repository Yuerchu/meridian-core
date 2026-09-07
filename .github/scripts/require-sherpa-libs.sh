#!/usr/bin/env bash
# Make Cargo re-download sherpa-onnx when the cache kept the bookkeeping but
# dropped the libraries.
#
# `sherpa-onnx-sys` unpacks prebuilt libraries under the target directory and,
# on any later build, returns early if the `lib` directory merely exists. The
# cache breaks that assumption invisibly: a restore brings back Cargo's record
# that the build script already ran, while the libraries themselves may not
# survive, since the cache action prunes the target directory before saving it.
#
# What follows is a link against a path that is present and empty —
#
#   ld: library 'sherpa-onnx-c-api' not found
#
# — or, on Windows, our own build script panicking over the DLLs it stages.
#
# So check the pairing the crate assumes and never verifies: if Cargo thinks
# the script has run, the libraries have to be there. When they are not, remove
# both halves so the script runs again and downloads afresh.
set -euo pipefail

target="target"
[ -d "$target" ] || { echo "no target directory yet"; exit 0; }

# Two possible homes for the unpack, and which one is used depends on how the
# job invokes cargo: plain `cargo test` puts it at target/sherpa-onnx-prebuilt,
# while `--target <triple>` moves it under target/<triple>/. Match both rather
# than assuming, which is the mistake this script previously made.
libs() {
  find "$target" -maxdepth 5 -type f -path '*/sherpa-onnx-prebuilt/*/lib/*' -print -quit 2>/dev/null
}

# Cargo splits its record in two — the fingerprint that decides whether to
# re-run, and the recorded output of the last run. Both are keyed per crate,
# and both sit under a profile directory whose depth varies with --target.
records() {
  find "$target" -type d -name 'sherpa-onnx-sys-*' -print -quit 2>/dev/null
}

if [ -n "$(libs)" ]; then
  echo "sherpa-onnx libraries present, leaving the cache alone"
  exit 0
fi

if [ -z "$(records)" ]; then
  echo "nothing built yet; the build script will download on its own"
  exit 0
fi

echo "cargo has a record of the build script but the libraries are gone — clearing both"
find "$target" -maxdepth 2 -type d -name sherpa-onnx-prebuilt -print0 2>/dev/null | xargs -0 -r rm -rf
find "$target" -type d -name 'sherpa-onnx-sys-*' -print0 2>/dev/null | xargs -0 -r rm -rf
echo "cleared"
