// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buffered file transfers and pipe-owned source consumption.

use alloc::{sync::Arc, vec};

use kerrno::{KError, KResult, LinuxError};

use crate::{FMode, MutexGuard, VfsFile, pipe::PipeObject};

/// Position policy for one side of a transfer.
pub enum TransferPosition<'a> {
    /// The engine reads `f_pos` under the position lock and commits the
    /// transferred count back to it, mirroring Linux `fdget_pos`: checks and
    /// copy share the critical section with the position they validated.
    Managed,
    /// An explicit offset: used as the transfer start and advanced by the
    /// transferred count; the open file description's `f_pos` is untouched.
    At(&'a mut u64),
}

/// One resolved side of a file pair: the open file description's `f_pos`
/// under its held lock, or a plain position. A `Managed` side of a
/// non-seekable file has no lockable position and resolves to an inert
/// scratch slot — stream operations ignore positions.
enum PositionSlot<'a> {
    Managed(MutexGuard<'a, u64>),
    At(&'a mut u64),
}

impl PositionSlot<'_> {
    /// The transfer start for this side.
    fn start(&self) -> u64 {
        match self {
            PositionSlot::Managed(guard) => **guard,
            PositionSlot::At(target) => **target,
        }
    }

    /// Publishes the transferred position; completion protocol only.
    fn commit(&mut self, value: u64) {
        match self {
            PositionSlot::Managed(guard) => **guard = value,
            PositionSlot::At(target) => **target = value,
        }
    }
}

/// A resolved file pair. Address-order lock acquisition, same-description
/// sharing and non-seekable inertness are settled once at resolution; the
/// engines below speak only in `(input, output)` terms.
enum PairPositions<'a> {
    Distinct {
        input: PositionSlot<'a>,
        output: PositionSlot<'a>,
    },
    /// Both sides `Managed` on one open file description: a single lock
    /// guards one logical position shared by the pair.
    Shared(PositionSlot<'a>),
}

impl PairPositions<'_> {
    /// The `(input, output)` transfer starts.
    fn starts(&self) -> (u64, u64) {
        match self {
            PairPositions::Distinct { input, output } => (input.start(), output.start()),
            PairPositions::Shared(slot) => {
                let start = slot.start();
                (start, start)
            }
        }
    }

    /// Completion protocol: publish the transferred positions.
    fn commit(&mut self, input_value: u64, output_value: u64) {
        match self {
            PairPositions::Distinct { input, output } => {
                input.commit(input_value);
                output.commit(output_value);
            }
            PairPositions::Shared(slot) => slot.commit(output_value),
        }
    }
}

/// Resolves a file pair's position protocol: `Managed` sides take their
/// position locks in address order, both-`Managed` sides of one open file
/// description share a single lock (one logical position), and `Managed`
/// sides of non-seekable files resolve to inert scratches. `At` sides keep
/// the caller's offset untouched here.
fn resolve_pair_positions<'a>(
    input: &'a Arc<VfsFile>,
    input_position: TransferPosition<'a>,
    output: &'a Arc<VfsFile>,
    output_position: TransferPosition<'a>,
    input_scratch: &'a mut u64,
    output_scratch: &'a mut u64,
) -> PairPositions<'a> {
    let input_at = match input_position {
        TransferPosition::Managed => None,
        TransferPosition::At(offset) => Some(offset),
    };
    let output_at = match output_position {
        TransferPosition::Managed => None,
        TransferPosition::At(offset) => Some(offset),
    };
    // A `Managed` side needs a lock exactly when its file is seekable.
    let input_lockable = input_at.is_none() && !input.is_stream();
    let output_lockable = output_at.is_none() && !output.is_stream();

    let same_description = Arc::ptr_eq(input, output);
    let (mut first_guard, mut second_guard) = if same_description {
        // One open file description: at most one lock, recorded where either
        // side can claim it. Two `Managed` sides are one logical position.
        let lockable = input_lockable || output_lockable;
        (lockable.then(|| input.position_lock()), None)
    } else {
        // Distinct descriptions lock in address order.
        let input_first = Arc::as_ptr(input) <= Arc::as_ptr(output);
        let (first, second) = if input_first {
            (input, output)
        } else {
            (output, input)
        };
        let (first_lockable, second_lockable) = if input_first {
            (input_lockable, output_lockable)
        } else {
            (output_lockable, input_lockable)
        };
        let first_guard = first_lockable.then(|| first.position_lock());
        let second_guard = second_lockable.then(|| second.position_lock());
        (first_guard, second_guard)
    };

    // Both sides `Managed` on one description: a single logical position
    // served by the single lock.
    if same_description && input_lockable && output_lockable {
        let guard = first_guard
            .take()
            .expect("shared description took one lock");
        return PairPositions::Shared(PositionSlot::Managed(guard));
    }

    // Which stored guard each side claims. One description: the single lock
    // sits in `first_guard` and either lockable side claims it. Distinct
    // files: the address-order placement, disjoint between the sides.
    let input_first = Arc::as_ptr(input) <= Arc::as_ptr(output);
    let input_claims_first = same_description || input_first;
    let output_claims_first = same_description || !input_first;
    let mut claim = |first: bool| -> MutexGuard<'a, u64> {
        let stored = if first {
            &mut first_guard
        } else {
            &mut second_guard
        };
        stored.take().expect("a lockable side owns its lock")
    };
    let input_slot = match (input_at, input_lockable) {
        (Some(offset), _) => PositionSlot::At(offset),
        (None, true) => PositionSlot::Managed(claim(input_claims_first)),
        (None, false) => PositionSlot::At(input_scratch),
    };
    let output_slot = match (output_at, output_lockable) {
        (Some(offset), _) => PositionSlot::At(offset),
        (None, true) => PositionSlot::Managed(claim(output_claims_first)),
        (None, false) => PositionSlot::At(output_scratch),
    };
    PairPositions::Distinct {
        input: input_slot,
        output: output_slot,
    }
}

/// Transfers up to `len` bytes, committing only bytes accepted by the destination.
///
/// [`TransferPosition::Managed`] sides use the open file descriptions'
/// positions, read and committed under the position locks; explicit offsets
/// are kernel values advanced by this operation, leaving `f_pos` untouched.
/// User-memory copyout and syscall restart decisions belong to the caller.
/// Errors after progress return the completed byte count; a zero-progress
/// wait preserves `IoWaitInterrupted`. File-to-file backend counts must fit
/// the supplied buffers. An oversized result is EIO and does not commit that
/// iteration's positions; earlier progress is retained. Backend data side
/// effects cannot be rolled back.
///
/// `nonblocking` controls pipe-readiness waits only; backend operations and
/// mutex acquisition may still block. Explicit offsets on streams, and a
/// non-pipe stream source with a non-pipe destination, return `ESPIPE`.
///
/// Runs in sleepable task context. Callers must not hold file-position or pipe
/// locks. Dispatch falls out by shape: pipe pairs to the pipe engine, a
/// single pipe side to its dedicated leg, and plain file pairs to the
/// buffered pair copy under `resolve_pair_positions`. File positions are
/// locked in address order; pipe paths acquire a non-pipe position only
/// inside the pipe-owned transfer, never across a pipe-readiness wait.
pub fn transfer_files(
    input: &Arc<VfsFile>,
    input_position: TransferPosition<'_>,
    output: &Arc<VfsFile>,
    output_position: TransferPosition<'_>,
    len: usize,
    nonblocking: bool,
) -> KResult<usize> {
    input.verify_mode(FMode::READ)?;
    output.verify_mode(FMode::WRITE)?;
    let source_pipe = PipeObject::from_file(input).ok();
    let destination_pipe = PipeObject::from_file(output).ok();
    if matches!(&input_position, TransferPosition::At(_)) && input.is_stream() {
        return Err(LinuxError::ESPIPE.into());
    }
    if matches!(&output_position, TransferPosition::At(_)) && output.is_stream() {
        return Err(LinuxError::ESPIPE.into());
    }
    if let (Some(source), Some(destination)) = (&source_pipe, &destination_pipe) {
        return source.splice_to(
            destination,
            len,
            nonblocking || input.is_nonblocking() || output.is_nonblocking(),
        );
    }
    // A non-seekable source needs a destination that accepts its entire read.
    // Pipe-to-file uses the pipe-owned consumption operation below instead.
    if input.is_stream() && source_pipe.is_none() && destination_pipe.is_none() {
        return Err(LinuxError::ESPIPE.into());
    }
    // One pipe side per leg: each leg owns its position protocol, because a
    // position lock is never held across a pipe-readiness wait.
    if let Some(pipe) = &destination_pipe {
        return splice_file_to_pipe(pipe, output, input, input_position, len, nonblocking);
    }
    if let Some(pipe) = &source_pipe {
        return splice_pipe_to_file(pipe, input, output, output_position, len, nonblocking);
    }
    copy_file_pair(input, input_position, output, output_position, len)
}

/// Pipe-destination leg: pulls from `input` into the pipe. The input's
/// `Managed` position re-locks per chunk, so the lock never spans a
/// pipe-readiness wait; the pipe side itself is always `Managed` (an `At`
/// position on a stream was rejected upstream).
fn splice_file_to_pipe(
    pipe: &PipeObject,
    output: &Arc<VfsFile>,
    input: &Arc<VfsFile>,
    input_position: TransferPosition<'_>,
    len: usize,
    nonblocking: bool,
) -> KResult<usize> {
    let input_managed = matches!(input_position, TransferPosition::Managed);
    let input_offset = match input_position {
        TransferPosition::Managed => None,
        TransferPosition::At(offset) => Some(offset),
    };
    let mut input_pos = input_offset.as_deref().copied().unwrap_or(0);
    let mut buf = vec![0; 0x1000]; // 4KB intermediate buffer
    let mut total_written = 0;
    let mut remaining = len;
    let mut result = Ok(0);
    while remaining > 0 {
        let count = buf.len().min(remaining);
        let transfer = pipe.splice_write(
            nonblocking || output.is_nonblocking() || total_written > 0,
            &mut buf[..count],
            |bytes| {
                if input_managed && !input.is_stream() {
                    input.read_from(bytes, &mut input.position_lock())
                } else {
                    input.read_from(bytes, &mut input_pos)
                }
            },
        );
        match transfer {
            Ok(written) => {
                total_written += written;
                remaining -= written;
                result = Ok(total_written);
                // Do not start a second blocking source read after publishing
                // stream data into the pipe; return that completed prefix.
                if written < count || input.is_stream() {
                    break;
                }
            }
            Err(error) => {
                result = if total_written > 0 {
                    Ok(total_written)
                } else {
                    Err(error)
                };
                break;
            }
        }
    }
    if let Some(offset) = input_offset {
        *offset = input_pos;
    }
    result
}

/// Pipe-source leg: drains the pipe into `output`. The output's `Managed`
/// position re-locks per chunk, so the lock never spans a pipe-readiness
/// wait; the pipe side itself is always `Managed` (an `At` position on a
/// stream was rejected upstream).
fn splice_pipe_to_file(
    pipe: &PipeObject,
    input: &Arc<VfsFile>,
    output: &Arc<VfsFile>,
    output_position: TransferPosition<'_>,
    len: usize,
    nonblocking: bool,
) -> KResult<usize> {
    let output_managed = matches!(output_position, TransferPosition::Managed);
    let output_offset = match output_position {
        TransferPosition::Managed => None,
        TransferPosition::At(offset) => Some(offset),
    };
    let mut output_pos = output_offset.as_deref().copied().unwrap_or(0);
    let mut buf = vec![0; 0x1000]; // 4KB intermediate buffer
    let mut total_written = 0;
    let mut remaining = len;
    let mut result = Ok(0);
    while remaining > 0 {
        let count = buf.len().min(remaining);
        let transfer = pipe.splice_read(
            nonblocking || input.is_nonblocking() || total_written > 0,
            &mut buf[..count],
            |bytes| {
                if output_managed && !output.is_stream() {
                    output.write_from(bytes, &mut output.position_lock())
                } else {
                    output.write_from(bytes, &mut output_pos)
                }
            },
        );
        match transfer {
            Ok(written) => {
                total_written += written;
                remaining -= written;
                result = Ok(total_written);
                if written < count {
                    break;
                }
            }
            Err(error) => {
                result = if total_written > 0 {
                    Ok(total_written)
                } else {
                    Err(error)
                };
                break;
            }
        }
    }
    if let Some(offset) = output_offset {
        *offset = output_pos;
    }
    result
}

/// File-pair leg: the buffered copy between two files, under the position
/// locks resolved by `resolve_pair_positions`. Both positions run on local
/// copies and commit through the resolved slots, so a destination that
/// refuses the tail rewinds the source for the next chunk.
fn copy_file_pair(
    input: &Arc<VfsFile>,
    input_position: TransferPosition<'_>,
    output: &Arc<VfsFile>,
    output_position: TransferPosition<'_>,
    len: usize,
) -> KResult<usize> {
    let mut input_scratch = 0;
    let mut output_scratch = 0;
    let mut positions = resolve_pair_positions(
        input,
        input_position,
        output,
        output_position,
        &mut input_scratch,
        &mut output_scratch,
    );
    let (mut input_pos, mut output_pos) = positions.starts();
    let mut buf = vec![0; 0x1000]; // 4KB intermediate buffer
    let mut total_written = 0;
    let mut remaining = len;
    let mut result = Ok(0);
    while remaining > 0 {
        let count = buf.len().min(remaining);
        let mut read_pos = input_pos;
        let transfer = input
            .read_from(&mut buf[..count], &mut read_pos)
            .and_then(|read| {
                if read > count {
                    return Err(KError::Io);
                }
                if read == 0 {
                    return Ok(0);
                }
                let mut write_pos = output_pos;
                let written = output.write_from(&buf[..read], &mut write_pos)?;
                if written > read {
                    return Err(KError::Io);
                }
                // The buffered suffix was never committed to the destination.
                input_pos += written as u64;
                output_pos = write_pos;
                Ok(written)
            });
        match transfer {
            Ok(written) => {
                total_written += written;
                remaining -= written;
                result = Ok(total_written);
                if written < count {
                    break;
                }
            }
            Err(error) => {
                result = if total_written > 0 {
                    Ok(total_written)
                } else {
                    Err(error)
                };
                break;
            }
        }
    }
    positions.commit(input_pos, output_pos);
    result
}

/// Runs the `copy_file_range(2)` prechecks and returns the request shortened
/// to the effective copy length, mirroring Linux `generic_copy_file_checks`
/// (`fs/read_write.c:1412`) plus `generic_file_rw_checks`: directories are
/// rejected with `EISDIR`, every non-regular fd (pipes, sockets, devices)
/// with `EINVAL`, a destination opened `O_APPEND` with `EBADF` (the copy
/// would land at the wrong offset), and a cross-superblock pair with `EXDEV`
/// unless both operation tables report the same native copy handler. The
/// range checks then run in Linux's order: an unsigned wrap of the request
/// is reported as `EOVERFLOW`, the request is shortened to the source EOF,
/// and the destination's own ceiling applies — a start at or past the
/// maximum file size is `EFBIG` and the count is clamped to what the
/// destination can hold. Overlapping ranges within one inode are rejected
/// with `EINVAL` so the chunked copy can never read bytes it has already
/// rewritten; a zero-length request never overlaps and remains a no-op.
fn verify_copy_file_range(
    input: &VfsFile,
    output: &VfsFile,
    input_start: u64,
    output_start: u64,
    requested: u64,
) -> KResult<u64> {
    let input_inode = input.inode();
    let output_inode = output.inode();
    if input_inode.node_type() == crate::NodeType::Directory
        || output_inode.node_type() == crate::NodeType::Directory
    {
        return Err(LinuxError::EISDIR.into());
    }
    if input_inode.node_type() != crate::NodeType::RegularFile
        || output_inode.node_type() != crate::NodeType::RegularFile
    {
        return Err(LinuxError::EINVAL.into());
    }
    if output.flags().contains(crate::OpenFlags::APPEND) {
        return Err(LinuxError::EBADF.into());
    }
    let input_super_block = input_inode.super_block()?;
    let output_super_block = output_inode.super_block()?;
    // Linux `generic_copy_file_checks` (fs/read_write.c:1436): a pair whose
    // tables carry the same native copy handler is admitted across super
    // blocks; every other pair needs the same super block.
    let same_super_block = Arc::ptr_eq(&input_super_block, &output_super_block);
    let same_handler = match (input.file_copy_handler(), output.file_copy_handler()) {
        (Some(input_handler), Some(output_handler)) => input_handler == output_handler,
        _ => false,
    };
    if !same_super_block && !same_handler {
        return Err(LinuxError::EXDEV.into());
    }
    // Linux reports `EOVERFLOW` when the range wraps its unsigned arithmetic
    // (`pos_in + count < pos_in || pos_out + count < pos_out`); the ABI layer
    // already rejected negative starts. Requests beyond the source EOF are
    // shortened below, not rejected.
    if input_start.checked_add(requested).is_none() || output_start.checked_add(requested).is_none()
    {
        return Err(LinuxError::EOVERFLOW.into());
    }
    // Shorten to the source EOF before the destination ceiling and the
    // overlap check, so a request past EOF is a no-op rather than a false
    // overlap. The size is snapshotted once: a concurrent truncate between
    // two reads would underflow the subtraction.
    let size = input.size();
    let count = if input_start >= size {
        0
    } else {
        requested.min(size - input_start)
    };
    // Linux `generic_write_check_limits`: the destination's own ceiling
    // applies in the checks phase — a start at or past the maximum file size
    // is `EFBIG` even for an already-empty request, and the count is clamped
    // to what the destination can hold from here.
    let output_max = output.max_file_size();
    if output_start >= output_max {
        return Err(LinuxError::EFBIG.into());
    }
    let count = count.min(output_max - output_start);
    // Overlapping ranges within one inode are rejected so the chunked copy
    // can never read bytes it has already rewritten; a zero-length request
    // never overlaps and remains a no-op success.
    if Arc::ptr_eq(input_inode, output_inode)
        && count != 0
        && input_start < output_start.saturating_add(count)
        && output_start < input_start.saturating_add(count)
    {
        return Err(LinuxError::EINVAL.into());
    }
    Ok(count)
}

/// `copy_file_range(2)` engine: the Linux checks and the buffered file-pair
/// copy in one call. [`TransferPosition::Managed`] sides have `f_pos` read,
/// validated and committed under the position locks — resolution, checks,
/// copy and the position commit share one critical section (Linux
/// `fdget_pos`), so concurrent transfers on a shared open file description
/// neither duplicate ranges nor clobber a concurrent seek. [`At`] sides run
/// at the given offsets and are advanced by the transferred count; `f_pos`
/// is untouched for them. Like Linux `vfs_copy_file_range`, the copy allows
/// partial success: an error after progress returns the completed prefix
/// and commits it, so the published positions always describe the bytes
/// the destination actually accepted.
///
/// `nonblocking` is accepted for symmetry with [`transfer_files`]; regular
/// file backends ignore it.
pub fn copy_file_range(
    input: &Arc<VfsFile>,
    input_position: TransferPosition<'_>,
    output: &Arc<VfsFile>,
    output_position: TransferPosition<'_>,
    len: usize,
    _nonblocking: bool,
) -> KResult<usize> {
    input.verify_mode(FMode::READ)?;
    output.verify_mode(FMode::WRITE)?;

    // The position protocol resolves once — address-order locks, shared
    // same-description locks and non-seekable inertness all live in
    // `resolve_pair_positions` — so the checks, the copy and the commit
    // below run against positions that share one critical section.
    let mut input_scratch = 0;
    let mut output_scratch = 0;
    let mut positions = resolve_pair_positions(
        input,
        input_position,
        output,
        output_position,
        &mut input_scratch,
        &mut output_scratch,
    );
    let (mut input_start, mut output_start) = positions.starts();

    let count = verify_copy_file_range(input, output, input_start, output_start, len as u64)?;

    let mut buf = vec![0; 0x1000]; // 4KB intermediate buffer
    let mut total_written = 0;
    let mut remaining = count;
    let mut result = Ok(0);
    while remaining > 0 {
        let count_chunk = (buf.len() as u64).min(remaining) as usize;
        let mut read_pos = input_start;
        let transfer = input
            .read_from(&mut buf[..count_chunk], &mut read_pos)
            .and_then(|read| {
                if read > count_chunk {
                    return Err(KError::Io);
                }
                if read == 0 {
                    return Ok((0, 0)); // EOF: stop without a destination write.
                }
                let mut write_pos = output_start;
                let written = output.write_from(&buf[..read], &mut write_pos)?;
                if written > read {
                    return Err(KError::Io);
                }
                // Only the accepted prefix commits to both positions: the
                // tail stays at the source for the next iteration to re-read.
                input_start += written as u64;
                output_start = write_pos;
                Ok((read, written))
            });
        match transfer {
            Ok((read, written)) => {
                total_written += written;
                remaining -= written as u64;
                result = Ok(total_written);
                // Stop at EOF or when the destination refuses the tail.
                if read == 0 || written < read {
                    break;
                }
            }
            Err(error) => {
                // copy_file_range allows partial success (Linux
                // `vfs_copy_file_range`): an error after progress returns
                // the completed prefix and the commit below publishes it.
                result = if total_written > 0 {
                    Ok(total_written)
                } else {
                    Err(error)
                };
                break;
            }
        }
    }

    // Position commit belongs to the transfer-completion protocol: publish
    // the transferred positions under the resolved slots, only after real
    // progress — a zero-byte return leaves `f_pos` alone.
    if total_written > 0 {
        positions.commit(input_start, output_start);
    }
    result
}
