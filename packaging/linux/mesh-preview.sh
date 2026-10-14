#!/usr/bin/env bash
# The mesh preview build of this fork: GridCraft with co-editing over the Macula mesh, under its
# own identity (io.macula.gridcraft-mesh, package and binary gridcraft-mesh) so it installs beside
# an upstream GridCraft and never passes as one. Arguments go to package.sh (e.g. --formats).
#
# Source paths are remapped, so the binaries name no build machine's directories.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export GRIDCRAFT_APP_ID=io.macula.gridcraft-mesh
export GRIDCRAFT_APP_NAME="GridCraft (mesh preview)"
export GRIDCRAFT_PKG=gridcraft-mesh
export GRIDCRAFT_MAINTAINER="${GRIDCRAFT_MAINTAINER:-Macula <raf.lefever@erlef.org>}"
export GRIDCRAFT_VENDOR=Macula
export GRIDCRAFT_HOMEPAGE=https://github.com/rgfaber/gridcraft
ROOT="$(cd "$HERE/../.." && pwd)"
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$CARGO_HOME=cargo --remap-path-prefix=$ROOT=gridcraft --remap-path-prefix=$HOME=home"
exec "$HERE/package.sh" "$@"
