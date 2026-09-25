# AArch64 / QEMU boot capture

Captured on 2026-09-24 from `/root/codes/x-kernel`, revision
`23367971fb49d9d2c0618f7d7086ac9d5902fcb9` (working tree reported dirty).

The numbered logs preserve stdout and stderr for the complete sequence:

1. `make clean`
2. Delete root-level `*.img` files, including `disk.img` and three downloaded images.
3. `cp platforms/kplat-aarch64/qemu_defconfig .config`
4. `make defconfig`
5. `make build`
6. `make rootfs` (required after deleting the disk; default Debian/OpenRC image)
7. `make run`

All commands succeeded. QEMU reached `root@kylin-x:~#`; the recorder then sent
QEMU's Ctrl-A X escape to stop the VM. The termination message remains in the log.
Warnings and all build output are preserved. The raw run log includes ANSI escape
sequences and `script` capture metadata.

Regenerate `session.json` with `python3 scripts/import-boot-recording.py` from the
website root. The website demo starts at the configuration copy, omits clean/image
deletion, selects key build/configuration messages, and retains the boot mascot
and guest initialization. It stops at the ready shell; exit status markers and
QEMU termination are omitted. Raw logs above remain complete and unchanged.
Playback timing is accelerated, not measured wall-clock timing.
