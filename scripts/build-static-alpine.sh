#!/bin/sh
set -eu

host=$(rustc -vV | awk '/^host:/ { print $2 }')
RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static"
export RUSTFLAGS
case "$host" in
  x86_64-alpine-linux-musl|x86_64-unknown-linux-musl)
    cargo build --release
    artifact=target/release/multi-server
    ;;
  *)
    if ! command -v rustup >/dev/null 2>&1; then
      echo "rustup is required to cross-compile from $host" >&2
      exit 1
    fi
    rustup target add x86_64-unknown-linux-musl
    cargo build --release --target x86_64-unknown-linux-musl
    artifact=target/x86_64-unknown-linux-musl/release/multi-server
    ;;
esac

if readelf -l "$artifact" | grep -q 'INTERP'; then
  echo "build is dynamically linked: $artifact" >&2
  exit 1
fi

mkdir -p dist
cp "$artifact" dist/multi-server
chmod 755 dist/multi-server
file dist/multi-server
du -h dist/multi-server
