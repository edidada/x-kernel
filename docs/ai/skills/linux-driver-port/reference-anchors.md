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
| 2026-11-05 07:01 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_work` | the HSE latch is fatal and survives a completed qTD |
| 2026-11-05 07:03 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_append_tds` | a transfer descriptor the controller may still own is never rewritten: dummy qTD / rotating slots |
| 2026-11-05 07:07 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_work` | USBSTS.REC: the controller has released the asynchronous schedule before software edits it |
| 2026-11-05 07:08 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_run` | the run bit gates all schedule fetches |
| 2026-11-05 07:11 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_refresh` | queue head overlay is the record of what the controller fetched |
| 2026-11-05 07:12 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_refresh` | a halted queue head keeps HALTED in its overlay until software clears it |
| 2026-11-05 07:15 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_work` | which register or descriptor the controller stops fetching |
| 2026-11-05 07:17 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_link_async` | every endpoint queue head stays in the asynchronous list; only its contents change |
| 2026-11-05 07:21 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_work` | the asynchronous ring links at the moment a transfer stops being fetched |
| 2026-11-05 07:23 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_link_async` | recovery rebuilds the whole ring, never truncates it to an empty list |
| 2026-11-05 07:24 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_link_async` | recovery rebuilds the ring instead of truncating it |
| 2026-11-05 07:25 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_refresh` | every queue head's overlay HALT is cleared, not only the list head |
| 2026-11-05 07:29 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_refresh` | full queue head state at the moment a transfer is not fetched |
| 2026-11-05 07:32 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-timer.c` | `ehci_poll_ASS` | ASS is polled; a stopped schedule is restarted with CMD_ASE, and no queue head is left holding an incomplete descriptor |
| 2026-11-05 09:45 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-timer.c` | `ehci_poll_ASS` | want/actual comparison of STS_ASS and CMD_ASE, and waiting for ASS to clear before enabling |
| 2026-11-05 09:46 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-timer.c` | `ehci_poll_ASS` | want/actual STS_ASS comparison and setting CMD_ASE when the schedule stopped |
| 2026-11-05 09:52 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_make` | queue head endpoint characteristics: endpoint number, speed, maximum packet size |
| 2026-11-05 10:23 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_refresh` | sampling the outstanding window instead of only the end state |
| 2026-11-05 10:24 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_link_async` | reclamation-list head must carry the H bit; it is what ends the list |
| 2026-11-05 10:29 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-timer.c` | `ehci_poll_ASS` | the schedule base is reloaded and ASS acknowledged across a stop/start |
| 2026-11-05 10:34 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-mem.c` | `ehci_mem_init` | periodic frame list allocated and programmed even when PSE is off |
| 2026-11-05 10:36 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `disable_async` | the controller is stopped outright when the asynchronous schedule goes idle |
| 2026-11-05 10:45 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_append_tds` | enqueue by replacing the dummy descriptor's contents; the queue head's qtd_next never changes |
| 2026-11-05 10:46 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_refresh` | the overlay halt is cleared before the descriptor becomes active |
| 2026-11-05 10:48 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_refresh` | an idle queue head is inactive, not halted |
| 2026-11-05 11:01 | `/workspace/linux-7.1.8/drivers/usb/storage/transport.c` | `usb_stor_Bulk_transport` | tag increment, learnt signature, zero-length and stalled CSW retries |
| 2026-11-05 11:03 | `/workspace/linux-7.1.8/drivers/usb/storage/transport.c` | `usb_stor_Bulk_transport` | skipped-data-phase CSW detection and the phase-error reset path |
| 2026-11-05 11:06 | `/workspace/linux-7.1.8/drivers/usb/host/ehci.h` | `struct ehci_qtd` | qTD field offsets: next(0) alt_next(4) token(8) buffer[5](12..31) |
| 2026-11-05 11:08 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_urb_enqueue` | every enqueue path starts from a running, unhalted controller |
| 2026-11-05 11:09 | `/workspace/linux-7.1.8/drivers/usb/storage/transport.c` | `usb_stor_Bulk_reset` | BOT reset recovery before first use |
| 2026-11-05 11:11 | `/workspace/linux-7.1.8/drivers/usb/core/hub.c` | `hub_port_init` | gaps between configuration and the first bulk traffic |
| 2026-11-05 11:18 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_async_advance` | IAAD doorbell forces the controller out of reclamation |
| 2026-11-05 11:21 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `scan_async` | a queue head ahead of the ring blocks every endpoint behind it |
| 2026-11-05 11:23 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-q.c` | `qh_completions` | observing whether the controller ever sees the descriptor's Active bit |
| 2026-11-05 11:25 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_irq` | the controller must react to a corrupted ring if it is walking it |
| 2026-11-05 11:32 | `/workspace/linux-7.1.8/drivers/clk/rockchip/clk-rk3588.c` | `CLKGATE_CON` | CRU clock-gate words can gate a host clock behind the driver's back |
| 2026-11-05 11:57 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.c` | `xhci_update_hub_device` | hub endpoint configuration: the slot must reach Configured before children are addressed |
| 2026-11-05 11:58 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-hub.c` | `xhci_add_hub_endpoint` | hub interrupt endpoint configured so the slot reaches Configured |
| 2026-11-05 11:58 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.c` | `xhci_add_endpoint` | adding the hub endpoint moves the slot to Configured |
| 2026-11-05 12:00 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.c` | `xhci_add_endpoint` | endpoint number in dword1, context index = ep*2+dir, entries cover it |
| 2026-11-05 12:01 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.c` | `xhci_add_endpoint` | context index = ep*2+dir; dword1 carries the endpoint number; entries cover it |
| 2026-11-05 12:11 | `/workspace/linux-7.1.8/drivers/usb/core/hub.c` | `hub_port_init` | the retry re-runs hub_port_reset before addressing again |
