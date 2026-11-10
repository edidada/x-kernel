# subsys-mmc — Security

## Trust boundaries

| Boundary | Input | Handling |
| --- | --- | --- |
| Card → protocol core | CID/CSD/SCR/switch-status bytes over the bus | All decodes are bounds-checked bit extraction; malformed registers degrade to `MmcError::Unsupported`, never to misaddressed I/O. |
| Card → capacity | CSD-derived `sectors` | Published as the Gendisk capacity; the block layer bounds every `block_id` against it. |
| Descriptor → device | card arguments (`arg`) | Constructed only from the card's own RCA/geometry; no firmware-controlled strings are formatted into device names (`mmcblk<slot>` uses the host slot index). |

## Invariants

- `csd_capacity` clamps unknown CSD structures to `Unsupported`; a hostile
  CSD cannot produce a capacity that overflows (`u64` shift bounded by the
  22/12-bit field widths).
- The SCR structure version is checked before use; unknown structures fail
  closed.
- `read_blocks`/`write_blocks` reject non-multiple buffer sizes and count
  overflow before any command is issued.

## Failure behavior

Every protocol failure maps to `MmcError` → `KError` at the block
boundary; detection failures leave the slot claimed but cardless (logged),
and I/O failures propagate to the block layer as `Io`/`WouldBlock`. No
failure path releases resources early: the card device is parented to the
host and the publication is Data-owned (drop-ordered), so a failing
transfer cannot orphan the disk or leak the host mapping.
