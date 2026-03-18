// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use num_enum::{FromPrimitive, IntoPrimitive};

// TA 命令 ID 定义
#[derive(FromPrimitive, IntoPrimitive)]
#[repr(u32)]
pub enum Command {
    GenerateBoxKey, // 生成保护箱密钥
    DestroyBoxKey,  // 销毁保护箱密钥
    BoxKeyIsExist,  // 检查密钥是否存在
    Encrypt,        // 加密数据
    Decrypt,        // 解密数据
    VerifyBoxKey,   // 验证密钥
    #[default]
    Unknown,
}

// If Uuid::parse_str() returns an InvalidLength error, there may be an extra
// newline in your uuid.txt file. You can remove it by running
// `truncate -s 36 uuid.txt`.
pub const UUID: &str = &include_str!("uuid.txt");
