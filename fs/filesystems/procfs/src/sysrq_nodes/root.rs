// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};

use kvfs::{NodePermission, VfsError, VfsResult};
use kvfs_simple::{DirMapping, RwFile, SimpleFile, SimpleFileOperation, SimpleFs};

const SYSRQ_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o200);
const SYSRQ_MAX_INPUT_LEN: usize = 16;

fn parse_sysrq_command(data: &[u8]) -> VfsResult<u8> {
    if data.is_empty() || data.len() > SYSRQ_MAX_INPUT_LEN {
        return Err(VfsError::InvalidInput);
    }
    let mut commands = data
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace());
    match (commands.next(), commands.next()) {
        (Some(b't'), None) => Ok(b't'),
        _ => Err(VfsError::InvalidInput),
    }
}

fn sysrq_trigger_write(data: &[u8]) -> VfsResult<()> {
    if !kthread::with_current_credentials(|credentials| credentials.is_privileged()) {
        return Err(VfsError::PermissionDenied);
    }

    match parse_sysrq_command(data)? {
        b't' => {
            ktask::snapshot::trigger("/proc/sysrq-trigger");
            Ok(())
        }
        _ => Err(VfsError::InvalidInput),
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "sysrq-trigger",
        SimpleFile::new_regular_with_permission(
            fs,
            SYSRQ_PERMISSION,
            RwFile::new(|op| match op {
                // Keep the synthetic backing content empty so every write is
                // delivered verbatim instead of being merged with read text.
                SimpleFileOperation::Read => Ok(Some(Vec::new())),
                SimpleFileOperation::Write(data) => {
                    sysrq_trigger_write(data)?;
                    Ok(None)
                }
            }),
        ),
    );
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::parse_sysrq_command;

    #[def_test]
    fn sysrq_parser_accepts_exactly_one_supported_command_byte() {
        assert_eq!(parse_sysrq_command(b" t\n").unwrap(), b't');
        assert!(parse_sysrq_command(b"").is_err());
        assert!(parse_sysrq_command(b" \n").is_err());
        assert!(parse_sysrq_command(b"tt").is_err());
        assert!(parse_sysrq_command(b"t x").is_err());
        assert!(parse_sysrq_command(b"x").is_err());
        assert!(parse_sysrq_command(&[b' '; 17]).is_err());
    }
}
