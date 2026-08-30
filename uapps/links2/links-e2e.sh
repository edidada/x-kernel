#!/bin/sh
# Optional guest e2e: keyboard TTY probe + two graphic restarts.
# Not used by autostart; the interactive demo execs Links after HTTP dump.
set -eu
kbd-probe
links-fb-verify
