#!/usr/bin/env bash
# Builds every download into target/dist from the release builds. Needs strip and mingw-w64,
# ImageMagick, NSIS (makensis) and nfpm. Run from the repository root.
set -euo pipefail

version=$(cargo metadata --no-deps --format-version 1 |
    python3 -I -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "connect"))')
export VERSION=$version
dist=target/dist
rm -rf "$dist"
mkdir -p "$dist/stage"

# These two names are what the in-app updater looks for.
x86_64-w64-mingw32-strip -o "$dist/connect.exe" target/x86_64-pc-windows-gnu/release/connect.exe
strip -o "$dist/connect-linux-x86_64" target/release/connect

makensis -V2 -DVERSION="$version" -DOUTPUT="..\\..\\$dist\\Connect-Installer.exe" packaging/windows/connect.nsi

cp "$dist/connect-linux-x86_64" "$dist/stage/connect"
for size in 16 24 32 48 64 128 256 512; do
    mkdir -p "$dist/stage/icons/${size}x${size}/apps"
    convert app/assets/icon/connect.png -resize "${size}x${size}" "$dist/stage/icons/${size}x${size}/apps/connect.png"
done
for format in deb rpm archlinux; do
    nfpm package --config packaging/linux/nfpm.yaml --packager "$format" --target "$dist/" >/dev/null
done
rm -rf "$dist/stage"

(cd "$dist" && sha256sum -- * >SHA256SUMS.txt && ls -la)
