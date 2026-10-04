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
# — or, on Windows, our own build script panicking over the DLLs it stages, or
# the crate's own "No shared runtime libraries found".
#
# So check the pairing the crate assumes and never verifies: if Cargo thinks
# the script has run, the libraries have to be there. When they are not, remove
# both halves so the script runs again and downloads afresh.
#
# **Every unpack, not the first one found.** There is one per target directory,
# and there is more than one target directory: plain `cargo test` uses
# target/, `--target <triple>` moves it under target/<triple>/, and trybuild
# compiles its cases in a target of its own under target/tests/trybuild/. This
# script used to stop at the first library it found — the main target's, kept
# by `cache-directories` — and so passed while trybuild's unpack sat restored
# and empty, failing `cap_compile_fail` on every run after the first.
set -euo pipefail

target="target"
[ -d "$target" ] || { echo "no target directory yet"; exit 0; }

cleared=0
clear_root() {
  local root=$1
  echo "cargo has a record of the build script under $root but its libraries are gone — clearing both"
  rm -rf "$root/sherpa-onnx-prebuilt"
  # Cargo splits its record in two — the fingerprint that decides whether to
  # re-run, and the recorded output of the last run — both keyed per crate,
  # under a profile directory whose depth varies with --target.
  find "$root" -type d -name 'sherpa-onnx-sys-*' -prune -exec rm -rf {} + 2>/dev/null || true
  cleared=1
}

found=0
while IFS= read -r -d '' lib; do
  found=1
  [ -d "$lib" ] || continue # cleared with an enclosing root already
  if [ -z "$(find "$lib" -maxdepth 1 -type f -print -quit 2>/dev/null)" ]; then
    # .../<root>/sherpa-onnx-prebuilt/<archive>/lib -> <root>
    clear_root "${lib%/sherpa-onnx-prebuilt/*}"
  fi
done < <(find "$target" -type d -path '*/sherpa-onnx-prebuilt/*/lib' -print0 2>/dev/null)

# No unpack anywhere, yet a record that the script ran: the libraries went
# with their whole directory.
if [ "$found" -eq 0 ] && [ -n "$(find "$target" -type d -name 'sherpa-onnx-sys-*' -print -quit 2>/dev/null)" ]; then
  clear_root "$target"
fi

if [ "$cleared" -eq 0 ]; then
  echo "every sherpa-onnx unpack has its libraries, leaving the cache alone"
else
  echo "cleared"
fi
