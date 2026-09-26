#!/usr/bin/env bash
# Build Mode B encode probe on Linux (Ubuntu box or CI).
set -euo pipefail

sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  pkg-config \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-ugly \
  gstreamer1.0-libav

cargo build -p broker --features modeb-encode --bins

echo
echo "Run (after editing broker/.env):"
echo "  cargo run -p broker --features modeb-encode --bin modeb_encode_probe"
echo "While running, check NVENC load:"
echo "  nvidia-smi dmon -s u"
echo "Output: /tmp/modeb-encoded.mp4"
