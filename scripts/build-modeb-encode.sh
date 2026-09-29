#!/usr/bin/env bash
# Build the broker with Mode B (GStreamer/NVENC) plus the Mode B probes on Linux (Ubuntu box or CI).
# Never runs sudo/apt — install GStreamer packages once on the box by hand.
set -euo pipefail

missing=0

if ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists gstreamer-1.0; then
  echo "ERROR: GStreamer development headers not found (pkg-config gstreamer-1.0)."
  missing=1
elif ! pkg-config --exists gstreamer-webrtc-1.0 gstreamer-sdp-1.0; then
  echo "ERROR: GStreamer WebRTC/SDP development headers not found (pkg-config gstreamer-webrtc-1.0 gstreamer-sdp-1.0)."
  missing=1
fi

if ! command -v gst-inspect-1.0 >/dev/null 2>&1 || ! gst-inspect-1.0 nvh264enc >/dev/null 2>&1; then
  echo "ERROR: nvh264enc not available via gst-inspect-1.0 (need nvcodec / plugins-bad + NVIDIA driver)."
  missing=1
fi

if [[ "${missing}" -ne 0 ]]; then
  cat <<'EOF'

Install once on the Ubuntu GPU box (as a user with apt privileges), then re-run:

  sudo apt-get update
  sudo apt-get install -y \
    pkg-config \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer-plugins-bad1.0-dev \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav \
    gstreamer1.0-nice

Verify:

  pkg-config --exists gstreamer-1.0 gstreamer-webrtc-1.0 gstreamer-sdp-1.0 && echo "gstreamer dev ok"
  gst-inspect-1.0 nvh264enc
  gst-inspect-1.0 webrtcbin

EOF
  exit 1
fi

echo "GStreamer present (gstreamer-1.0 + webrtc/sdp + nvh264enc); building broker + Mode B probes"
cargo build -p broker --features modeb-encode --bin broker --bin modeb_encode_probe --bin modeb_webrtc_probe --locked

echo
echo "Run (after editing broker/.env or exporting RDP_*):"
echo "  cargo run -p broker --features modeb-encode --bin broker               # Mode C on :7171, Mode B at ws://<MANAGEMENT_ADDR>/modeb/webrtc"
echo "  cargo run -p broker --features modeb-encode --bin modeb_encode_probe   # MP4 -> /tmp/modeb-encoded.mp4"
echo "  cargo run -p broker --features modeb-encode --bin modeb_webrtc_probe   # deprecated single-session probe"
echo "While running, check NVENC load:"
echo "  nvidia-smi dmon -s u"
