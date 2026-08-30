#!/bin/sh
# Guest helper for the Links 2.30 framebuffer demo.
set -eu

echo "=== input devices ==="
ls -l /dev/input || true

echo "=== mousedev-ps2 ==="
mousedev-ps2

echo "=== fb-mmap-refresh ==="
fb-mmap-refresh

echo "=== links -dump https://example.com ==="
xklinks -dump https://example.com
echo "https dump ok"

echo "=== links -dump CJK file ==="
xklinks -dump file:///usr/share/links-demo/cjk.html
echo "cjk dump ok"

echo "=== links -g CJK file ==="
exec xklinks -g file:///usr/share/links-demo/cjk.html
