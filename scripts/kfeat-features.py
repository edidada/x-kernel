#!/usr/bin/env python3
"""Derive the cargo feature set (kfeat/...) from .config, like xkmake does."""
import sys

SKIP = {"FS", "VIRTIO_BUS_PCI", "VIRTIO_BUS_MMIO"}
ARCHES = (
    ("MACHINE_AARCH64", "platform_kplat_aarch64"),
    ("MACHINE_RISCV64", "platform_kplat_riscv64"),
    ("MACHINE_X86_64", "platform_kplat_x86_64"),
    ("MACHINE_LOONGARCH64", "platform_kplat_loongarch64"),
)


def main() -> None:
    feats = []
    with open(".config", encoding="utf-8") as config:
        for line in config:
            line = line.strip()
            if line.startswith("KFEAT_") and line.endswith("=y"):
                name = line[6:-2]
                if name not in SKIP:
                    feats.append("kfeat/" + name.lower())
            else:
                for prefix, feature in ARCHES:
                    if line.startswith(prefix):
                        feats.append("kfeat/" + feature)
    print(",".join(feats))


if __name__ == "__main__":
    main()
