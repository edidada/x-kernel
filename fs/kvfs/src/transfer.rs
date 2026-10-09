// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buffered file transfers and pipe-owned source consumption.

use alloc::{sync::Arc, vec};

use kerrno::{KError, KResult, LinuxError};

use crate::{FMode, VfsFile, pipe::PipeObject};

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
/// locks. File positions are locked in address order; pipe paths acquire a
/// non-pipe position only inside the pipe-owned transfer, never across a
/// pipe-readiness wait.
#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: TODO(cg): legacy file transfer; split position-lock \
              setup, the per-chunk copy step and position write-back"
)]
pub fn transfer_files(
    input: &Arc<VfsFile>,
    input_position: TransferPosition<'_>,
    output: &Arc<VfsFile>,
    output_position: TransferPosition<'_>,
    len: usize,
    nonblocking: bool,
) -> KResult<usize> {
    let (input_offset, input_managed) = match input_position {
        TransferPosition::Managed => (None, true),
        TransferPosition::At(offset) => (Some(offset), false),
    };
    let (output_offset, output_managed) = match output_position {
        TransferPosition::Managed => (None, true),
        TransferPosition::At(offset) => (Some(offset), false),
    };

    input.verify_mode(FMode::READ)?;
    output.verify_mode(FMode::WRITE)?;
    let source_pipe = PipeObject::from_file(input).ok();
    let destination_pipe = PipeObject::from_file(output).ok();
    if input_offset.is_some() && input.is_stream() {
        return Err(LinuxError::ESPIPE.into());
    }
    if output_offset.is_some() && output.is_stream() {
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
    // File-to-file copies lock positions in address order, sharing one guard
    // for the same open description. Pipe paths acquire the non-pipe position
    // only inside the transfer callback, never across a pipe-readiness wait.
    let is_file_pair = source_pipe.is_none() && destination_pipe.is_none();
    let input_first = Arc::as_ptr(input) <= Arc::as_ptr(output);
    let (first, second) = if input_first {
        (input, output)
    } else {
        (output, input)
    };
    let mut first_pos = (is_file_pair && !first.is_stream()).then(|| first.position_lock());
    let mut second_pos = (is_file_pair && !Arc::ptr_eq(first, second) && !second.is_stream())
        .then(|| second.position_lock());
    let first_start = first_pos.as_deref().copied().unwrap_or(0);
    let second_start = if Arc::ptr_eq(first, second) {
        first_start
    } else {
        second_pos.as_deref().copied().unwrap_or(0)
    };
    let (input_start, output_start) = if input_first {
        (first_start, second_start)
    } else {
        (second_start, first_start)
    };
    let mut input_pos = input_offset.as_deref().copied().unwrap_or(input_start);
    let mut output_pos = output_offset.as_deref().copied().unwrap_or(output_start);
    let mut buf = vec![0; 0x1000]; // 4KB intermediate buffer
    let mut total_written = 0;
    let mut remaining = len;
    let mut result = Ok(0);
    while remaining > 0 {
        let count = buf.len().min(remaining);
        let transfer = if let Some(pipe) = &destination_pipe {
            pipe.splice_write(
                nonblocking || output.is_nonblocking() || total_written > 0,
                &mut buf[..count],
                |bytes| {
                    if input_managed && !input.is_stream() {
                        input.read_from(bytes, &mut input.position_lock())
                    } else {
                        input.read_from(bytes, &mut input_pos)
                    }
                },
            )
        } else if let Some(pipe) = &source_pipe {
            pipe.splice_read(
                nonblocking || input.is_nonblocking() || total_written > 0,
                &mut buf[..count],
                |bytes| {
                    if output_managed && !output.is_stream() {
                        output.write_from(bytes, &mut output.position_lock())
                    } else {
                        output.write_from(bytes, &mut output_pos)
                    }
                },
            )
        } else {
            let mut read_pos = input_pos;
            input
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
                })
        };
        match transfer {
            Ok(written) => {
                total_written += written;
                remaining -= written;
                result = Ok(total_written);
                // Do not start a second blocking source read after publishing
                // stream data into the pipe; return that completed prefix.
                if written < count || (input.is_stream() && source_pipe.is_none()) {
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
    if input_managed {
        let position = if input_first {
            &mut first_pos
        } else {
            &mut second_pos
        };
        if let Some(position) = position {
            **position = input_pos;
        }
    }
    if output_managed {
        let position = if !input_first || Arc::ptr_eq(input, output) {
            &mut first_pos
        } else {
            &mut second_pos
        };
        if let Some(position) = position {
            **position = output_pos;
        }
    }
    drop(second_pos);
    drop(first_pos);
    if let Some(offset) = input_offset {
        *offset = input_pos;
    }
    if let Some(offset) = output_offset {
        *offset = output_pos;
    }
    result
}

/// Runs the `copy_file_range(2)` prechecks and returns the request shortened
/// to the source EOF, mirroring Linux `generic_file_rw_checks`
/// (`fs/read_write.c:1720`) plus `generic_copy_file_checks`: directories are
/// rejected with `EISDIR`, every non-regular fd (pipes, sockets, devices)
/// with `EINVAL`, a destination opened `O_APPEND` with `EBADF` (the copy
/// would land at the wrong offset), a cross-superblock pair with `EXDEV`
/// unless both operation tables report file-copy support (Linux admits pairs
/// whose `->copy_file_range` handlers match), and overlapping ranges within
/// one inode with `EINVAL` so the chunked copy can never read bytes it has
/// already rewritten. Offset overflow against the original request is
/// reported as `EOVERFLOW` before the EOF shortening, so an overflowing
/// request cannot masquerade as a valid no-op. A start at or past EOF — or a
/// request longer than the remaining bytes — yields a zero-length no-op.
pub fn verify_copy_file_range(
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
    let same_super_block = Arc::ptr_eq(&input_super_block, &output_super_block);
    let copy_supported = input.supports_file_copy() && output.supports_file_copy();
    if !same_super_block && !copy_supported {
        return Err(LinuxError::EXDEV.into());
    }
    if input_start.checked_add(requested).is_none() || output_start.checked_add(requested).is_none()
    {
        return Err(LinuxError::EOVERFLOW.into());
    }
    // Shorten to the source EOF before the overlap check, so a request past
    // EOF is a no-op rather than a false overlap. The size is snapshotted
    // once: a concurrent truncate between two reads would underflow the
    // subtraction.
    let size = input.size();
    let count = if input_start >= size {
        0
    } else {
        requested.min(size - input_start)
    };
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
/// is untouched for them.
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
    let input_managed = matches!(input_position, TransferPosition::Managed);
    let output_managed = matches!(output_position, TransferPosition::Managed);
    let mut input_at = match input_position {
        TransferPosition::Managed => None,
        TransferPosition::At(offset) => Some(offset),
    };
    let mut output_at = match output_position {
        TransferPosition::Managed => None,
        TransferPosition::At(offset) => Some(offset),
    };

    // Position locks in address order; one lock covers a same-description
    // pair. Only `Managed` sides participate — `At` sides carry their own
    // offsets and leave `f_pos` alone.
    let same_file = Arc::ptr_eq(input, output);
    let input_first = Arc::as_ptr(input) <= Arc::as_ptr(output);
    let (first, second) = if input_first {
        (input, output)
    } else {
        (output, input)
    };
    let first_needs_lock = if input_first {
        input_managed
    } else {
        output_managed
    };
    let second_needs_lock = if input_first {
        output_managed
    } else {
        input_managed
    };
    let mut first_guard = first_needs_lock.then(|| first.position_lock());
    let mut second_guard = (!same_file && second_needs_lock).then(|| second.position_lock());
    let first_start = first_guard.as_ref().map_or(0, |guard| **guard);
    let second_start = second_guard.as_ref().map_or(first_start, |guard| **guard);
    let (mut input_start, mut output_start) = if input_first {
        (first_start, second_start)
    } else {
        (second_start, first_start)
    };
    if let Some(offset) = &mut input_at {
        input_start = **offset;
    }
    if let Some(offset) = &mut output_at {
        output_start = **offset;
    }

    let count = verify_copy_file_range(input, output, input_start, output_start, len as u64)?;

    let mut buf = vec![0; 0x1000]; // 4KB intermediate buffer
    let mut total_written = 0;
    let mut remaining = count.min(len as u64);
    while remaining > 0 {
        let count_chunk = (buf.len() as u64).min(remaining) as usize;
        let mut read_pos = input_start;
        let read = input.read_from(&mut buf[..count_chunk], &mut read_pos)?;
        if read == 0 {
            break; // EOF reached.
        }
        let mut write_pos = output_start;
        let written = output.write_from(&buf[..read], &mut write_pos)?;
        if written > read {
            return Err(KError::Io);
        }
        // Only the accepted prefix commits to both positions: the tail stays
        // at the source for the next iteration to re-read.
        input_start += written as u64;
        output_start = write_pos;
        total_written += written;
        remaining -= written as u64;
        if written < read {
            break;
        }
    }

    // Position commit belongs to the transfer-completion protocol: publish
    // the transferred positions under the held locks, only after real
    // progress — a failure or zero-byte return leaves `f_pos` alone.
    if total_written > 0 {
        if input_managed {
            let position = if input_first {
                first_guard.as_mut()
            } else {
                second_guard.as_mut()
            };
            if let Some(position) = position {
                **position = input_start;
            }
        }
        if output_managed {
            let position = if !input_first || same_file {
                first_guard.as_mut()
            } else {
                second_guard.as_mut()
            };
            if let Some(position) = position {
                **position = output_start;
            }
        }
        // `At` sides advance the caller's offsets; `f_pos` is untouched.
        if let Some(offset) = &mut input_at {
            **offset = input_start;
        }
        if let Some(offset) = &mut output_at {
            **offset = output_start;
        }
    }
    Ok(total_written)
}
