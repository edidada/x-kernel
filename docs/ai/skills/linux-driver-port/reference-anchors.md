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
| 2026-11-05 12:35 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.c` | `xhci_get_endpoint_flag` | add-flags bits cover only the endpoint being added; EP0 is never re-added |
| 2026-11-05 12:52 | `/workspace/linux-7.1.8/drivers/pmdomain/rockchip/pm-domains.c` | `rockchip_pd_power` | PMU mutex + already-on early return: every driver goes through one locked implementation |
| 2026-11-05 13:16 | `/workspace/linux-7.1.8/drivers/pci/controller/dwc/pcie-dw-rockchip.c` | `ltssm` | a link that never trained has no DBI window; polling for linkup before touching it |
| 2026-11-05 13:21 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.c` | `xhci_update_hub_device` | command 1: DEV_HUB + MAX_PORTS, add_flags = SLOT |
| 2026-11-05 13:21 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.c` | `xhci_add_endpoint` | command 2: add_flags = SLOT | ep_flag, entries raised |
| 2026-11-05 14:03 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.h` | `xhci_last_valid_endpoint` | entries must cover contiguous contexts up to the highest added index |
| 2026-11-05 14:04 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.h` | `xhci_last_valid_endpoint` | entries cover contiguous contexts; gaps are filled disabled first |
| 2026-11-05 14:20 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.h` | `struct xhci_ep_ctx / MAX_BURST` | ep_info2 has NO endpoint-number field: CErr 2:1, EP_TYPE 5:3, Max Burst 15:8, Max Packet 31:16; the endpoint number lives only in the context index |
| 2026-11-05 14:20 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-mem.c` | `xhci_get_endpoint_max_burst` | SS: ss_ep_comp.bMaxBurst; HS isoc/int: maxp_mult-1; otherwise 0 |
| 2026-11-05 14:20 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-mem.c` | `xhci_get_endpoint_interval` | SS/HS int-isoc: bInterval-1 exponent; FS/LS int: fls(8*bInterval)-1 clamped 3..10; FS isoc: exponent+3; ctrl/bulk: 0 |
| 2026-11-05 14:20 | `/workspace/linux-7.1.8/drivers/usb/core/usb.c` | `usb_endpoint_max_periodic_payload` | ESIT payload: ss_ep_comp.wBytesPerInterval for SS, max_packet*maxp_mult otherwise, 0 for ctrl/bulk |
| 2026-11-05 14:20 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-mem.c` | `xhci_endpoint_init` | ep ctx fill sequence: ep_info=ESIT_HI|INTERVAL|MULT, ep_info2=TYPE|MAX_PACKET|MAX_BURST|ERROR_COUNT(3), tx_info=ESIT_LO|AVG with avg_trb_len=max_esit_payload (ctrl: 8 on xHCI>=1.0) |
| 2026-11-05 14:20 | `/workspace/linux-7.1.8/drivers/usb/core/config.c` | `usb_parse_ss_endpoint_companion` | companion 0x30 sits right after its endpoint; missing/malformed falls back to an all-zero descriptor |
| 2026-11-05 15:00 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.h` | `TRB_RESET_EP` | Reset Endpoint = 14; 15 is Stop Endpoint (TRB_STOP_RING) |
| 2026-11-05 15:00 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-caps.h` | `HCS_MAX_SCRATCHPAD` | count = (bits25:21)<<5 | (bits31:27); register positions are reversed vs the field names |
| 2026-11-05 15:00 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.h` | `xhci_write_64 / xhci_add_interrupter` | generic 64-bit writes are lo_hi_writeq; ERSTBA alone flips to hi_lo when WRITE_64_HI_LO is set |
| 2026-11-05 15:00 | `/workspace/linux-7.1.8/drivers/usb/host/xhci.c` | `xhci_reset / xhci_halt` | halt wait XHCI_MAX_HALT_USEC=32ms; HCRST self-clear and CNR drop use the caller timeout (short=250ms) |
| 2026-11-05 15:00 | `/workspace/linux-7.1.8/include/uapi/linux/usb/ch11.h` | `USB_PORT_STAT_C_BH_RESET / USB_PORT_STAT_LINK_STATE` | C_BH_RESET change bit5 0x0020; SS link state bits 8:5 mask 0x01e0 |
| 2026-11-05 15:00 | `/workspace/linux-7.1.8/drivers/usb/core/hub.h` | `hub_power_on_good_delay` | external hub power-on wait = max(bPwrOn2PwrGood*2ms, 100ms) |
| 2026-11-05 15:10 | `/workspace/linux-7.1.8/drivers/usb/core/hub.c` | `hub_port_reset warm branch / hub_port_disable / hub_set_port_link_state` | BH reset = SetFeature(28), completion C_BH_RESET(29); SS disable = link to Rx.Detect (wIndex = (LS<<8)|port); no routine U0 write after reset |
| 2026-11-05 15:23 | `/workspace/linux-7.1.8/drivers/usb/host/ehci-hcd.c` | `ehci_urb_dequeue / start_unlink_async` | async list must stay intact; IAAD forces the controller out of reclamation to re-walk the list |
| 2026-11-05 16:31 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `xhci_handle_event` | event-path restructure: dispatch in ring order, FIFO commands, halt cleanup |
| 2026-11-05 16:31 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `handle_cmd_completion` | event-path restructure: dispatch in ring order, FIFO commands, halt cleanup |
| 2026-11-05 16:31 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `xhci_halted_host_endpoint` | event-path restructure: dispatch in ring order, FIFO commands, halt cleanup |
| 2026-11-05 16:31 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `xhci_handle_halted_endpoint` | event-path restructure: dispatch in ring order, FIFO commands, halt cleanup |
| 2026-11-05 16:31 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `xhci_handle_cmd_reset_ep` | event-path restructure: dispatch in ring order, FIFO commands, halt cleanup |
| 2026-11-05 16:31 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `xhci_handle_cmd_set_deq` | event-path restructure: dispatch in ring order, FIFO commands, halt cleanup |
| 2026-11-05 16:37 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `td_to_noop` | halted-TD cleanup: no-op the cached TD, then move the dequeue with SET_DEQ_PENDING |
| 2026-11-05 16:37 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `xhci_invalidate_cancelled_tds` | halted-TD cleanup: no-op the cached TD, then move the dequeue with SET_DEQ_PENDING |
| 2026-11-05 16:37 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `xhci_move_dequeue_past_td` | halted-TD cleanup: no-op the cached TD, then move the dequeue with SET_DEQ_PENDING |
| 2026-11-05 16:37 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `trb_to_noop` | halted-TD cleanup: no-op the cached TD, then move the dequeue with SET_DEQ_PENDING |
| 2026-11-05 16:42 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `xhci_urb_dequeue` | timeout path: cancel the URB (Stop Endpoint) and reconcile the ring before returning |
| 2026-11-05 16:42 | `/workspace/linux-7.1.8/drivers/usb/host/xhci-ring.c` | `xhci_handle_cmd_stop_ep` | timeout path: cancel the URB (Stop Endpoint) and reconcile the ring before returning |
