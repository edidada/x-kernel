#!/bin/sh
# Run Links 2.30 with CJK font, fontconfig, and built-in CA certificates.
set -eu

export FONTCONFIG_FILE=/etc/fonts/fonts.conf
export FONTCONFIG_PATH=/etc/fonts

FONT=/usr/share/fonts/truetype/wqy/wqy-microhei.ttc
# Headings use the bold face. An empty -font-bold falls back to the internal
# bitmap font, which has no CJK glyphs (they render as '*').
# Do not pass "path @ index": Links 2.30 still hands the original string
# (including " @ 1") to FT_New_Face, so the face fails to open.
exec /usr/local/bin/links \
  -font "${FONT}" \
  -font-bold "${FONT}" \
  -font-monospaced "${FONT}" \
  -font-monospaced-bold "${FONT}" \
  -font-italic "${FONT}" \
  -font-italic-bold "${FONT}" \
  -font-monospaced-italic "${FONT}" \
  -font-monospaced-italic-bold "${FONT}" \
  -ssl.builtin-certificates 1 \
  -html-assume-codepage utf-8 \
  "$@"
