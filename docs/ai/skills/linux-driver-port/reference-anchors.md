# Reference anchors

Every entry is one `scripts/ref-anchor.sh` run: the reference function that was
read before the driver code it aligns. `scripts/ref-anchor.sh --check` refuses to
run the board or to commit driver changes without a fresh entry, so this table
is the record of which mechanisms were transcribed rather than reconstructed.

| when (UTC) | reference | symbol | note |
|---|---|---|---|
| 2026-11-05 06:15 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_urb_transaction` | EHCI queue head / transfer descriptor structure |
| 2026-11-05 06:15 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_make` | EHCI queue head / transfer descriptor structure |
| 2026-11-05 06:15 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_link_async` | EHCI queue head / transfer descriptor structure |
| 2026-11-05 06:15 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hub.c` | `check_reset_complete` | port reset completion and companion handoff |
| 2026-11-05 06:15 | `/workspace/linux-7.1.8/drivers/usb/host/ehci.h` | `QTD_STS_ACTIVE` | qTD token field layout |
| 2026-11-05 06:15 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_urb_transaction` | bulk data toggle rule: flip after whole-packet transfers |
| 2026-11-05 06:18 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_append_tds` | queuing a transfer onto a queue head that the controller may still own |
| 2026-11-05 06:19 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_urb_transaction` | empirical check: per-transfer vs per-packet data toggle |
| 2026-11-05 06:20 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_update` | bulk endpoints: hardware owns the data toggle; QH_TOGGLE_CTL is control-only |
| 2026-11-05 06:22 | `/workspace/linux-7.1.8/fs/ext4/super.c` | `ext4_fill_super` | ext4 superblock location and magic |
| 2026-11-05 06:26 | `/workspace/linux-7.1.8/fs/ext4/ext4.h` | `EXT4_SUPER_MAGIC` | ext4 superblock/group-descriptor/extent/dirent layout for the listing probe |
| 2026-11-05 06:26 | `/workspace/linux-7.1.8/fs/ext4/inode.c` | `ext4_iget` | inode field offsets (i_mode, i_size_lo/hi) |
| 2026-11-05 06:27 | `/workspace/linux-7.1.8/fs/ext4/inode.c` | `ext4_iget` | inode field offsets; block size passed in rather than assumed |
| 2026-11-05 06:29 | `/workspace/linux-7.1.8/drivers/usb/storage/transport.c` | `usb_stor_Bulk_transport` | SB-2: data stage direction is taken from the CBW flags |
| 2026-11-05 06:37 | `/workspace/linux-7.1.8/fs/super.c` | `get_tree_bdev` | mounting a block device at a path: source path -> dev_t -> BlockDevice |
| 2026-11-05 06:38 | `/workspace/linux-7.1.8/fs/super.c` | `mount_bdev` | get_tree_bdev: a block device already carrying the root is not mounted twice |
| 2026-11-05 06:39 | `/workspace/linux-7.1.8/fs/ext4/super.c` | `ext4_fill_super` | why a mount attempt is refused (journal / features) |
| 2026-11-05 06:42 | `/workspace/linux-7.1.8/drivers/base/core.c` | `device_add` | device-node publication: what reaches /dev after device_add |
| 2026-11-05 06:45 | `/workspace/linux-7.1.8/drivers/base/devtmpfs.c` | `devtmpfs_create_node` | creating the /dev node for a published disk |
| 2026-11-05 06:46 | `/workspace/linux-7.1.8/fs/ext4/inode.c` | `ext4_map_blocks` | which block reads a directory walk issues |
| 2026-11-05 06:49 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `ehci_handle_halted_endpoint` | endpoint halt after a failed transfer |
| 2026-11-05 06:51 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_reset` | clearing a latched Host System Error and restarting the schedule |
| 2026-11-05 06:54 | `/workspace/linux-7.1.8/drivers/usb/storage/transport.c` | `usb_stor_Bulk_reset` | BOT soft reset: class reset then CLEAR_FEATURE(ENDPOINT_HALT) on both bulk endpoints |
| 2026-11-05 06:56 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_link_async` | the queue head handed to the asynchronous schedule |
| 2026-11-05 06:57 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_work` | recovering a controller that latched a halt before new work is queued |
| 2026-11-05 06:58 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_urb_enqueue` | every enqueue path clears a latched halt first |
