// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
//
// This file has been modified by KylinSoft on 2025.

#![allow(unused_assignments)]

use crate::api::tee_api_mm::{TEE_CheckMemoryAccessRights, TEE_Free, TEE_Malloc, TEE_MemFill};
use crate::api::tee_api_objects::{
    __utee_from_attr, TEE_AllocateTransientObject, TEE_CopyObjectAttributes1,
    TEE_FreeTransientObject, TEE_GetObjectInfo1, TEE_ResetTransientObject,
};
use crate::api::tee_api_panic::TEE_Panic;
use crate::syscalls::syscall_table::{
    _utee_asymm_operate, _utee_asymm_verify, _utee_authenc_dec_final, _utee_authenc_enc_final,
    _utee_authenc_init, _utee_authenc_update_aad, _utee_authenc_update_payload, _utee_cipher_final,
    _utee_cipher_init, _utee_cipher_update, _utee_cryp_derive_key, _utee_cryp_obj_get_info,
    _utee_cryp_random_number_generate, _utee_cryp_state_alloc, _utee_cryp_state_copy,
    _utee_cryp_state_free, _utee_hash_final, _utee_hash_init, _utee_hash_update,
};

use crate::tee_api_defines::*;
use crate::tee_api_types::{
    TEE_Attribute, TEE_ObjectHandle, TEE_ObjectInfo, TEE_OperationHandle, TEE_OperationInfo,
    TEE_OperationInfoKey, TEE_OperationInfoMultiple, TEE_Result,
};

use std::ptr;

/// 根据算法标识符获取算法类别
///
/// # 参数
/// - `alg`: 算法标识符
///
/// # 返回值
/// 返回算法类别，例如 TEE_OPERATION_DIGEST、TEE_OPERATION_CIPHER 等
///
/// # 算法标识符解析规则
/// - 算法类别位于第24-31位（右移24位）
pub fn TEE_ALG_GET_CLASS(alg: u32) -> u32 {
    (alg >> 24) & 0xFF
}

/// 根据算法标识符获取对应的密钥类型
///
/// # 参数
/// - `alg`: 算法标识符，包含算法类型和具体算法信息
/// - `with_private_key`: 是否包含私钥标识位（true表示保留私钥位，false表示清除私钥位）
///
/// # 返回值
/// 返回对应的密钥类型常量值，可能的值包括：
/// - 对称密钥类型：`TEE_TYPE_AES`、`TEE_TYPE_DES`、`TEE_TYPE_DES3`、`TEE_TYPE_SM4`
/// - HMAC类型：`TEE_TYPE_HMAC_MD5`、`TEE_TYPE_HMAC_SHA1`等
/// - 非对称密钥对类型：`TEE_TYPE_RSA_KEYPAIR`、`TEE_TYPE_DSA_KEYPAIR`等
/// - 默认类型：`TEE_TYPE_GENERIC_SECRET`
///
/// # 算法标识符解析规则
/// - 主要算法类型位于第17-24位（右移16位后取低8位）
/// - 具体算法类型位于末8位
/// - 私钥标识位位于最高位（当`with_private_key`为false时会被清除）
///
/// # 注意事项
/// - 对于不支持的算法类型，将返回`TEE_TYPE_GENERIC_SECRET`
/// - 当`with_private_key`为false时，会清除密钥类型中的私钥标识位
pub fn TEE_ALG_GET_KEY_TYPE(alg: u32, with_private_key: bool) -> u32 {
    // 从算法标识符中提取主要算法部分（位于第17-24位）
    let main_alg = (alg >> 16) & 0xFF;

    // 根据主要算法类型确定对应的密钥类型
    let key_type = match main_alg {
        0x10 | 0x11 | 0x13 | 0x14 => {
            // 对称加密算法：AES(0x10), DES(0x11), DES3(0x13), SM4(0x14)
            match main_alg {
                0x10 => TEE_TYPE_AES,         // AES算法
                0x11 => TEE_TYPE_DES,         // DES算法
                0x13 => TEE_TYPE_DES3,        // 3DES算法
                0x14 => TEE_TYPE_SM4,         // SM4算法
                _ => TEE_TYPE_GENERIC_SECRET, // 默认情况
            }
        }
        0x30 => {
            // HMAC算法，根据算法末8位确定具体类型
            match alg & 0xFF {
                0x01 => TEE_TYPE_HMAC_MD5,    // HMAC-MD5
                0x02 => TEE_TYPE_HMAC_SHA1,   // HMAC-SHA1
                0x03 => TEE_TYPE_HMAC_SHA224, // HMAC-SHA224
                0x04 => TEE_TYPE_HMAC_SHA256, // HMAC-SHA256
                0x05 => TEE_TYPE_HMAC_SHA384, // HMAC-SHA384
                0x06 => TEE_TYPE_HMAC_SHA512, // HMAC-SHA512
                0x07 => TEE_TYPE_HMAC_SM3,    // HMAC-SM3
                _ => TEE_TYPE_GENERIC_SECRET, // 默认情况
            }
        }
        0x70 | 0x80 => {
            // 非对称算法，根据算法末8位确定具体类型
            match alg & 0xFF {
                0x30 => TEE_TYPE_RSA_KEYPAIR,     // RSA密钥对
                0x31 => TEE_TYPE_DSA_KEYPAIR,     // DSA密钥对
                0x41 => TEE_TYPE_ECDSA_KEYPAIR,   // ECDSA密钥对
                0x42 => TEE_TYPE_ECDH_KEYPAIR,    // ECDH密钥对
                0x45 => TEE_TYPE_SM2_DSA_KEYPAIR, // SM2 DSA密钥对
                _ => TEE_TYPE_GENERIC_SECRET,     // 默认情况
            }
        }
        _ => TEE_TYPE_GENERIC_SECRET, // 不支持的算法类型
    };

    // 如果需要私钥，则返回完整的密钥类型，否则清除私钥位（保留公钥或对称密钥）
    if with_private_key {
        key_type
    } else {
        key_type & 0x0FFFFFFF // 清除私钥标识位，只保留公钥或对称密钥类型
    }
}

impl TEE_OperationHandle {
    /// 创建新的操作句柄
    pub fn new(
        info: TEE_OperationInfo,
        key1: TEE_ObjectHandle,
        key2: TEE_ObjectHandle,
        operation_state: u32,
        block_size: usize,
        state: u32,
    ) -> Self {
        TEE_OperationHandle {
            info,
            key1,
            key2,
            operation_state,
            buffer: core::ptr::null_mut(),
            buffer_two_blocks: false,
            block_size,
            buffer_offs: 0,
            state,
        }
    }

    /// 释放缓冲区内存
    pub fn free_buffer(&mut self) {
        if !self.buffer.is_null() {
            TEE_Free(self.buffer as *mut core::ffi::c_void);
            self.buffer = ptr::null_mut();
            self.buffer_offs = 0;
        }
    }
}

impl Drop for TEE_OperationHandle {
    fn drop(&mut self) {
        self.free_buffer();
    }
}

/// 缓冲区操作错误类型
#[derive(Debug, PartialEq)]
pub enum BufferError {
    BufferNotAllocated,
    InsufficientSpace,
    AllocationFailed,
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferError::BufferNotAllocated => write!(f, "Buffer not allocated"),
            BufferError::InsufficientSpace => write!(f, "Insufficient space in buffer"),
            BufferError::AllocationFailed => write!(f, "Memory allocation failed"),
        }
    }
}

impl std::error::Error for BufferError {}

impl TEE_OperationHandle {
    /// 分配缓冲区内存
    pub fn allocate_buffer_v2(&mut self, size: usize) -> std::result::Result<(), BufferError> {
        // 使用TEE_Malloc进行内存分配
        let new_buffer = TEE_Malloc(size, TEE_MALLOC_FILL_ZERO);
        if new_buffer.is_null() {
            return Err(BufferError::AllocationFailed);
        }

        // 释放旧缓冲区
        if !self.buffer.is_null() {
            TEE_Free(self.buffer as *mut core::ffi::c_void);
        }

        self.buffer = new_buffer as *mut u8;
        self.buffer_offs = 0;
        Ok(())
    }
}

/// 操作配置结果
#[derive(Debug, PartialEq)]
enum OperationConfig {
    Digest {
        block_size: usize,
    },
    Cipher {
        block_size: usize,
        buffer_two_blocks: bool,
        req_key_usage: u32,
        with_private_key: bool,
    },
    AsymmetricSignature {
        req_key_usage: u32,
        with_private_key: bool,
    },
    AsymmetricEncryption {
        req_key_usage: u32,
        with_private_key: bool,
    },
    KeyDerivation {
        req_key_usage: u32,
    },
    Mac {
        req_key_usage: u32,
    },
}

/// 验证算法参数并获取配置
fn validate_algorithm_params(
    algorithm: u32,
    mode: u32,
    max_key_size: u32,
) -> Result<OperationConfig, TEE_Result> {
    use crate::tee_api_defines::*;

    // 验证密钥大小
    match algorithm {
        TEE_ALG_DSA_SHA1 => {
            if max_key_size < 512 || max_key_size > 1024 || max_key_size % 64 != 0 {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
        }
        TEE_ALG_DSA_SHA224 => {
            if max_key_size != 2048 {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
        }
        TEE_ALG_DSA_SHA256 => {
            if max_key_size != 2048 && max_key_size != 3072 {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
        }
        TEE_ALG_ECDSA_P192 | TEE_ALG_ECDH_P192 => {
            if max_key_size != 192 {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
        }
        TEE_ALG_ECDSA_P224 | TEE_ALG_ECDH_P224 => {
            if max_key_size != 224 {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
        }
        TEE_ALG_ECDSA_P256 | TEE_ALG_ECDH_P256 | TEE_ALG_SM2_PKE | TEE_ALG_SM2_DSA_SM3 => {
            if max_key_size != 256 {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
        }
        TEE_ALG_SM2_KEP => {
            if max_key_size != 512 {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
        }
        TEE_ALG_ECDSA_P384 | TEE_ALG_ECDH_P384 => {
            if max_key_size != 384 {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
        }
        TEE_ALG_ECDSA_P521 | TEE_ALG_ECDH_P521 => {
            if max_key_size != 521 {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
        }
        _ => {}
    }

    // 根据算法类型返回配置
    match algorithm {
        // 摘要算法
        TEE_ALG_MD5 | TEE_ALG_SHA1 | TEE_ALG_SHA224 | TEE_ALG_SHA256 | TEE_ALG_SHA384
        | TEE_ALG_SHA512 | TEE_ALG_SM3 => {
            if mode != TEE_MODE_DIGEST {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
            let digest_length = match algorithm & 0x000000FF {
                0x01 => 16, // MD5
                0x02 => 20, // SHA1
                0x03 => 28, // SHA224
                0x04 => 32, // SHA256
                0x05 => 48, // SHA384
                0x06 => 64, // SHA512
                0x10 => 32, // SM3
                _ => 0,
            };
            Ok(OperationConfig::Digest {
                block_size: digest_length,
            })
        }

        // MAC 算法
        TEE_ALG_DES_CBC_MAC_NOPAD
        | TEE_ALG_AES_CBC_MAC_NOPAD
        | TEE_ALG_AES_CBC_MAC_PKCS5
        | TEE_ALG_AES_CMAC
        | TEE_ALG_DES_CBC_MAC_PKCS5
        | TEE_ALG_DES3_CBC_MAC_NOPAD
        | TEE_ALG_DES3_CBC_MAC_PKCS5
        | TEE_ALG_HMAC_MD5
        | TEE_ALG_HMAC_SHA1
        | TEE_ALG_HMAC_SHA224
        | TEE_ALG_HMAC_SHA256
        | TEE_ALG_HMAC_SHA384
        | TEE_ALG_HMAC_SHA512
        | TEE_ALG_HMAC_SM3 => {
            if mode != TEE_MODE_MAC {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
            Ok(OperationConfig::Mac {
                req_key_usage: TEE_USAGE_MAC,
            })
        }

        // 对称加密算法
        TEE_ALG_AES_ECB_NOPAD
        | TEE_ALG_AES_CBC_NOPAD
        | TEE_ALG_AES_CCM
        | TEE_ALG_DES_ECB_NOPAD
        | TEE_ALG_DES_CBC_NOPAD
        | TEE_ALG_DES3_ECB_NOPAD
        | TEE_ALG_DES3_CBC_NOPAD
        | TEE_ALG_SM4_ECB_NOPAD
        | TEE_ALG_SM4_CBC_NOPAD
        | TEE_ALG_SM4_CTR => {
            let main_alg = (algorithm & 0x00FF0000) >> 16;
            let block_size = match main_alg {
                0x1000 => 16, // TEE_MAIN_ALGO_AES
                0x4000 => 16, // TEE_MAIN_ALGO_SM4
                _ => 8,
            };
            Ok(OperationConfig::Cipher {
                block_size,
                buffer_two_blocks: false,
                req_key_usage: 0,        // 稍后根据 mode 设置
                with_private_key: false, // 稍后根据 mode 设置
            })
        }

        // 需要两个块的算法
        TEE_ALG_AES_CTS => {
            let main_alg = (algorithm & 0x00FF0000) >> 16;
            let block_size = match main_alg {
                0x1000 => 16, // TEE_MAIN_ALGO_AES
                0x4000 => 16, // TEE_MAIN_ALGO_SM4
                _ => return Err(TEE_ERROR_NOT_SUPPORTED),
            };
            Ok(OperationConfig::Cipher {
                block_size,
                buffer_two_blocks: true,
                req_key_usage: 0,
                with_private_key: false,
            })
        }

        // CTR 和 GCM 模式
        TEE_ALG_AES_CTR | TEE_ALG_AES_GCM => {
            let block_size = 16;
            match mode {
                TEE_MODE_ENCRYPT => Ok(OperationConfig::Cipher {
                    block_size,
                    buffer_two_blocks: false,
                    req_key_usage: TEE_USAGE_ENCRYPT,
                    with_private_key: false,
                }),
                TEE_MODE_DECRYPT => Ok(OperationConfig::Cipher {
                    block_size,
                    buffer_two_blocks: false,
                    req_key_usage: TEE_USAGE_DECRYPT,
                    with_private_key: true,
                }),
                _ => Err(TEE_ERROR_NOT_SUPPORTED),
            }
        }

        // 非对称签名算法
        TEE_ALG_ECDSA_P192 | TEE_ALG_ECDSA_P224 | TEE_ALG_ECDSA_P256 | TEE_ALG_ECDSA_P384
        | TEE_ALG_ECDSA_P521 | TEE_ALG_SM2_DSA_SM3 => match mode {
            TEE_MODE_SIGN => Ok(OperationConfig::AsymmetricSignature {
                req_key_usage: TEE_USAGE_SIGN,
                with_private_key: true,
            }),
            TEE_MODE_VERIFY => Ok(OperationConfig::AsymmetricSignature {
                req_key_usage: TEE_USAGE_VERIFY,
                with_private_key: false,
            }),
            _ => Err(TEE_ERROR_NOT_SUPPORTED),
        },

        // 非对称加密算法
        TEE_ALG_RSAES_PKCS1_V1_5
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384
        | TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512
        | TEE_ALG_SM2_PKE => match mode {
            TEE_MODE_ENCRYPT => Ok(OperationConfig::AsymmetricEncryption {
                req_key_usage: TEE_USAGE_ENCRYPT,
                with_private_key: false,
            }),
            TEE_MODE_DECRYPT => Ok(OperationConfig::AsymmetricEncryption {
                req_key_usage: TEE_USAGE_DECRYPT,
                with_private_key: true,
            }),
            _ => Err(TEE_ERROR_NOT_SUPPORTED),
        },

        // RSA NOPAD
        TEE_ALG_RSA_NOPAD => match mode {
            TEE_MODE_ENCRYPT => Ok(OperationConfig::AsymmetricEncryption {
                req_key_usage: TEE_USAGE_ENCRYPT | TEE_USAGE_VERIFY,
                with_private_key: false,
            }),
            TEE_MODE_DECRYPT => Ok(OperationConfig::AsymmetricEncryption {
                req_key_usage: TEE_USAGE_DECRYPT | TEE_USAGE_SIGN,
                with_private_key: true,
            }),
            _ => Err(TEE_ERROR_NOT_SUPPORTED),
        },

        // 密钥派生算法
        TEE_ALG_DH_DERIVE_SHARED_SECRET
        | TEE_ALG_ECDH_P192
        | TEE_ALG_ECDH_P224
        | TEE_ALG_ECDH_P256
        | TEE_ALG_ECDH_P384
        | TEE_ALG_ECDH_P521
        | TEE_ALG_SM2_KEP => {
            if mode != TEE_MODE_DERIVE {
                return Err(TEE_ERROR_NOT_SUPPORTED);
            }
            Ok(OperationConfig::KeyDerivation {
                req_key_usage: TEE_USAGE_DERIVE,
            })
        }

        _ => Err(TEE_ERROR_NOT_SUPPORTED),
    }
}

/// 分配并初始化一个TEE加密操作句柄
///
/// 该函数用于创建并初始化一个加密操作句柄，支持多种加密算法和操作模式。
///
/// # 参数
/// - `operation`: 输出参数，接收新创建的操作句柄指针
/// - `algorithm`: 指定加密算法 (TEE_ALG_*)
/// - `mode`: 指定操作模式 (TEE_MODE_*)
/// - `max_key_size`: 密钥的最大长度(以比特为单位)
///
/// # 返回值
/// 返回TEE_Result状态码:
/// - TEE_SUCCESS: 操作成功
/// - TEE_ERROR_OUT_OF_MEMORY: 内存分配失败
/// - TEE_ERROR_BAD_PARAMETERS: 无效的算法或模式参数
/// - TEE_ERROR_NOT_SUPPORTED: 不支持的算法或模式
///
/// # 安全性
/// - 调用者必须确保`operation`是有效的指针
/// - 返回的操作句柄必须最终通过TEE_FreeOperation释放
///
/// # Panics
/// - 如果传入的`operation`参数为空指针，将触发TEE_Panic
///
/// # 注意
/// - 对于非摘要操作，会自动分配临时密钥对象
/// - 对于SM2 KEP算法，会特殊处理双密钥情况
/// - 操作句柄的内存会被初始化为零
/// TEE 操作分配函数
#[unsafe(no_mangle)]
pub extern "C" fn TEE_AllocateOperation(
    operation: *mut *mut TEE_OperationHandle,
    algorithm: u32,
    mode: u32,
    max_key_size: u32,
) -> TEE_Result {
    use crate::tee_api_defines::*;

    // 检查空指针
    if operation.is_null() {
        TEE_Panic(0);
        return TEE_ERROR_GENERIC;
    }

    // 验证算法参数
    let config = match validate_algorithm_params(algorithm, mode, max_key_size) {
        Ok(c) => c,
        Err(e) => return e,
    };

    // 获取算法相关信息
    let operation_class = (algorithm & 0xFF000000) >> 24;
    let digest_length = match algorithm & 0x000000FF {
        0x01 => 16, // MD5
        0x02 => 20, // SHA1
        0x03 => 28, // SHA224
        0x04 => 32, // SHA256
        0x05 => 48, // SHA384
        0x06 => 64, // SHA512
        0x10 => 32, // SM3
        _ => 0,
    };
    let _main_alg = (algorithm & 0x00FF0000) >> 16;

    let mut handle_state = 0u32;

    // 检查是否需要两个密钥
    if algorithm == TEE_ALG_SM2_KEP {
        handle_state = TEE_HANDLE_FLAG_EXPECT_TWO_KEYS;
    }

    let (block_size, buffer_two_blocks, req_key_usage, with_private_key) = match config {
        OperationConfig::Digest { block_size } => {
            handle_state |= TEE_HANDLE_FLAG_KEY_SET;
            (block_size, false, 0, false)
        }
        OperationConfig::Cipher {
            block_size,
            buffer_two_blocks,
            req_key_usage,
            with_private_key,
        } => (
            block_size,
            buffer_two_blocks,
            req_key_usage,
            with_private_key,
        ),
        OperationConfig::AsymmetricSignature {
            req_key_usage,
            with_private_key,
        } => (1, false, req_key_usage, with_private_key),
        OperationConfig::AsymmetricEncryption {
            req_key_usage,
            with_private_key,
        } => (1, false, req_key_usage, with_private_key),
        OperationConfig::KeyDerivation { req_key_usage } => (1, false, req_key_usage, true),
        OperationConfig::Mac { req_key_usage } => (1, false, req_key_usage, false),
    };

    // 创建操作信息
    let op_info = TEE_OperationInfo {
        algorithm,
        operationClass: operation_class,
        mode,
        digestLength: digest_length,
        maxKeySize: max_key_size,
        keySize: 0,
        requiredKeyUsage: req_key_usage,
        handleState: handle_state,
    };

    // 分配操作句柄结构
    let operation_ptr = TEE_Malloc(
        core::mem::size_of::<TEE_OperationHandle>(),
        TEE_MALLOC_FILL_ZERO,
    );
    if operation_ptr.is_null() {
        return TEE_ERROR_OUT_OF_MEMORY;
    }

    // 初始化操作句柄
    let op_handle = unsafe { &mut *(operation_ptr as *mut TEE_OperationHandle) };
    *op_handle = TEE_OperationHandle::new(
        op_info,
        ptr::null_mut(),
        ptr::null_mut(),
        TEE_OPERATION_STATE_INITIAL,
        block_size,
        0,
    );

    // 分配缓冲区
    if block_size > 1 {
        let buffer_size = if buffer_two_blocks {
            block_size * 2
        } else {
            block_size
        };
        if op_handle.allocate_buffer_v2(buffer_size).is_err() {
            TEE_Free(operation_ptr);
            return TEE_ERROR_OUT_OF_MEMORY;
        }
    }
    op_handle.buffer_two_blocks = buffer_two_blocks;

    // 检查是否需要密钥（非摘要操作）
    if operation_class != TEE_OPERATION_DIGEST {
        let mut mks = max_key_size;
        let key_type = TEE_ALG_GET_KEY_TYPE(algorithm, with_private_key);

        if handle_state & TEE_HANDLE_FLAG_EXPECT_TWO_KEYS != 0 {
            mks /= 2;
        }

        let mut key1_ptr = core::ptr::null_mut();
        let res = TEE_AllocateTransientObject(key_type, mks, &mut key1_ptr);
        if res != TEE_SUCCESS {
            TEE_Free(operation_ptr);
            return res;
        }
        op_handle.key1 = key1_ptr;

        if handle_state & TEE_HANDLE_FLAG_EXPECT_TWO_KEYS != 0 {
            let mut key2_ptr = core::ptr::null_mut();
            let res = TEE_AllocateTransientObject(key_type, mks, &mut key2_ptr);
            if res != TEE_SUCCESS {
                TEE_FreeTransientObject(op_handle.key1);
                TEE_Free(operation_ptr);
                return res;
            }
            op_handle.key2 = key2_ptr;
        }
    }

    // 分配加密状态
    let mut state = 0u32;
    let res = unsafe {
        _utee_cryp_state_alloc(
            algorithm as u64,
            mode as u64,
            op_handle.key1 as u64,
            op_handle.key2 as u64,
            &mut state,
        )
    };
    if res != TEE_SUCCESS as usize {
        TEE_FreeTransientObject(op_handle.key1);
        TEE_FreeTransientObject(op_handle.key2);
        TEE_Free(operation_ptr);
        return res as TEE_Result;
    }
    op_handle.state = state;

    // 初始化摘要操作
    if operation_class == TEE_OPERATION_DIGEST {
        let res = unsafe { _utee_hash_init(state as u64, core::ptr::null(), 0) };
        if res != TEE_SUCCESS as usize {
            TEE_FreeTransientObject(op_handle.key1);
            TEE_FreeTransientObject(op_handle.key2);
            TEE_Free(operation_ptr);
            return res as TEE_Result;
        }
        op_handle.state |= TEE_HANDLE_FLAG_INITIALIZED;
    }

    op_handle.operation_state = TEE_OPERATION_STATE_INITIAL;

    // 将操作句柄写入输出参数
    unsafe {
        *operation = operation_ptr as *mut TEE_OperationHandle;
    }

    TEE_SUCCESS
}

/// 释放操作句柄
///
/// 释放操作句柄及其相关资源
#[unsafe(no_mangle)]
pub extern "C" fn TEE_FreeOperation(operation: *mut TEE_OperationHandle) {
    use crate::tee_api_defines::*;

    // 检查空句柄
    if operation.is_null() {
        return;
    }

    // 读取操作句柄结构
    let op_handle = unsafe { &*operation };

    // 释放缓冲区
    if !op_handle.buffer.is_null() {
        TEE_Free(op_handle.buffer as *mut core::ffi::c_void);
    }
    /*
     * Note that keys should not be freed here, since they are
     * claimed by the operation they will be freed by
     * utee_cryp_state_free().
     */
    // 释放加密状态
    if op_handle.state != 0 {
        let res = unsafe { _utee_cryp_state_free(op_handle.state as u64) };
        if res != TEE_SUCCESS as usize {
            TEE_Panic(res as u32);
        }
    }

    // 释放密钥对象，C语言中没有释放秘钥对象的操作。
    // if !op_handle.key1.is_null() {
    //     TEE_FreeTransientObject(op_handle.key1);
    // }
    // if !op_handle.key2.is_null() {
    //     TEE_FreeTransientObject(op_handle.key2);
    // }

    TEE_Free(op_handle.buffer as *mut core::ffi::c_void);

    // 释放操作句柄本身
    TEE_Free(operation as *mut core::ffi::c_void);
}

/// 获取操作信息
///
/// # 参数
/// * `operation` - 操作句柄
/// * `operationInfo` - 输出参数，用于接收操作信息
///
/// # 注意
/// 如果操作句柄为空，函数将触发 panic
#[unsafe(no_mangle)]
pub extern "C" fn TEE_GetOperationInfo(
    operation: *mut TEE_OperationHandle,
    operationInfo: *mut TEE_OperationInfo,
) {
    use crate::tee_api_defines::*;

    // 检查空句柄
    if operation.is_null() {
        TEE_Panic(0);
    }

    // 检查输出指针
    if operationInfo.is_null() {
        TEE_Panic(0);
    }

    // 检查输出参数的内存访问权限
    if cfg!(feature = "strict_annotation_checks") {
        let res = TEE_CheckMemoryAccessRights(
            TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
            operationInfo as *mut core::ffi::c_void,
            std::mem::size_of::<TEE_OperationInfo>(),
        );
        if res != 0 {
            eprintln!("[out] operationInfo: error {:#010x}", res);
            TEE_Panic(0);
        }
    }

    // 从操作句柄中获取操作信息
    let op_handle = unsafe { &*operation };
    unsafe {
        *operationInfo = op_handle.info;
    }

    // 如果操作期望两个密钥，清空密钥相关信息
    unsafe {
        if (*operationInfo).handleState & TEE_HANDLE_FLAG_EXPECT_TWO_KEYS != 0 {
            (*operationInfo).keySize = 0;
            (*operationInfo).requiredKeyUsage = 0;
        }
    }
}

/// 获取操作信息（多密钥版本）
///
/// # 参数
/// * `operation` - 操作句柄
/// * `op_info` - 输出参数，用于接收操作信息
/// * `size` - 指向缓冲区大小的指针
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_SHORT_BUFFER` - 缓冲区太小
#[unsafe(no_mangle)]
pub extern "C" fn TEE_GetOperationInfoMultiple(
    operation: *mut TEE_OperationHandle,
    op_info: *mut TEE_OperationInfoMultiple,
    size: *mut usize,
) -> TEE_Result {
    use crate::tee_api_defines::*;

    // 检查空句柄
    if operation.is_null() {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查输出参数
    if op_info.is_null() || size.is_null() {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查输出参数的内存访问权限
    if cfg!(feature = "strict_annotation_checks") {
        let buffer_size = unsafe { *size };
        let res = TEE_CheckMemoryAccessRights(
            TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
            op_info as *mut core::ffi::c_void,
            buffer_size,
        );
        if res != 0 {
            eprintln!("[out] op_info: error {:#010x}", res);
            TEE_Panic(0);
        }

        let res = TEE_CheckMemoryAccessRights(
            TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
            size as *mut core::ffi::c_void,
            std::mem::size_of::<usize>(),
        );
        if res != 0 {
            eprintln!("[out] size: error {:#010x}", res);
            TEE_Panic(0);
        }
    }

    // 检查缓冲区大小是否足够
    let op_info_size = std::mem::size_of::<TEE_OperationInfoMultiple>();
    let buffer_size = unsafe { *size };
    if buffer_size < op_info_size {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 计算最大密钥数量
    let key_info_size = std::mem::size_of::<TEE_OperationInfoKey>();
    let max_key_count = (buffer_size - op_info_size) / key_info_size;

    // 清空输出缓冲区
    TEE_MemFill(op_info as *mut core::ffi::c_void, 0, buffer_size);

    // 获取操作句柄
    let op_handle = unsafe { &mut *operation };
    let two_keys = (op_handle.info.handleState & TEE_HANDLE_FLAG_EXPECT_TWO_KEYS) != 0;

    let result = if op_handle.info.mode == TEE_MODE_DIGEST {
        // 摘要操作不需要密钥
        unsafe {
            (*op_info).numberOfKeys = 0;
        }
        TEE_SUCCESS
    } else if !two_keys {
        // 单密钥情况
        if max_key_count < 1 {
            return TEE_ERROR_SHORT_BUFFER;
        }

        let mut kinfo = TEE_ObjectInfo {
            objectType: 0,
            objectSize: 0,
            maxObjectSize: 0,
            objectUsage: 0,
            dataSize: 0,
            dataPosition: 0,
            handleFlags: 0,
        };

        let res = TEE_GetObjectInfo1(op_handle.key1, &mut kinfo);
        if res != TEE_SUCCESS {
            return check_operation_info_multiple_result(res);
        }

        unsafe {
            (*op_info)
                .keyInformation
                .add(0)
                .write(TEE_OperationInfoKey {
                    keySize: kinfo.objectSize,
                    requiredKeyUsage: op_handle.info.requiredKeyUsage,
                });
            (*op_info).numberOfKeys = 1;
        }

        TEE_SUCCESS
    } else {
        // 双密钥情况
        if max_key_count < 2 {
            return TEE_ERROR_SHORT_BUFFER;
        }

        let mut kinfo = TEE_ObjectInfo {
            objectType: 0,
            objectSize: 0,
            maxObjectSize: 0,
            objectUsage: 0,
            dataSize: 0,
            dataPosition: 0,
            handleFlags: 0,
        };

        // 获取第一个密钥信息
        let res = TEE_GetObjectInfo1(op_handle.key1, &mut kinfo);
        if res != TEE_SUCCESS {
            return check_operation_info_multiple_result(res);
        }

        unsafe {
            (*op_info)
                .keyInformation
                .add(0)
                .write(TEE_OperationInfoKey {
                    keySize: kinfo.objectSize,
                    requiredKeyUsage: op_handle.info.requiredKeyUsage,
                });
        }

        // 获取第二个密钥信息
        let res = TEE_GetObjectInfo1(op_handle.key2, &mut kinfo);
        if res != TEE_SUCCESS {
            return check_operation_info_multiple_result(res);
        }

        unsafe {
            (*op_info)
                .keyInformation
                .add(1)
                .write(TEE_OperationInfoKey {
                    keySize: kinfo.objectSize,
                    requiredKeyUsage: op_handle.info.requiredKeyUsage,
                });
            (*op_info).numberOfKeys = 2;
        }

        TEE_SUCCESS
    };

    // 如果成功，复制操作信息
    if result == TEE_SUCCESS {
        unsafe {
            (*op_info).algorithm = op_handle.info.algorithm;
            (*op_info).operationClass = op_handle.info.operationClass;
            (*op_info).mode = op_handle.info.mode;
            (*op_info).digestLength = op_handle.info.digestLength;
            (*op_info).maxKeySize = op_handle.info.maxKeySize;
            (*op_info).handleState = op_handle.info.handleState;
            (*op_info).operationState = op_handle.operation_state;
        }
    }

    check_operation_info_multiple_result(result)
}

/// 检查 TEE_GetOperationInfoMultiple 的返回结果
fn check_operation_info_multiple_result(res: TEE_Result) -> TEE_Result {
    if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
        TEE_Panic(res as u32);
    }
    res
}

/// 重置操作状态
///
/// 此函数重置操作的内部状态，将操作状态恢复为初始状态。
/// 缓冲区偏移量会被重置，但密钥和操作句柄状态不会被清除。
///
/// # 参数
/// * `operation` - 操作句柄
///
/// # 注意
/// - 如果操作句柄为空，函数将触发 panic
/// - 此函数只重置状态，不释放资源
fn reset_operation_state(operation: &mut TEE_OperationHandle) {
    // 重置操作状态为初始状态
    operation.operation_state = TEE_OPERATION_STATE_INITIAL;
    // 重置缓冲区偏移量
    operation.buffer_offs = 0;

    if operation.info.operationClass == TEE_OPERATION_DIGEST {
        let res = unsafe { _utee_hash_init(operation.state as u64, core::ptr::null(), 0) };
        if res != TEE_SUCCESS as usize {
            TEE_Panic(res as u32);
        }
        operation.info.handleState |= TEE_HANDLE_FLAG_INITIALIZED;
    } else {
        operation.info.handleState &= !TEE_HANDLE_FLAG_INITIALIZED;
    }
}

/// 重置操作
///
/// # 参数
/// * `operation` - 操作句柄
///
/// # 注意
/// 如果操作句柄为空，函数将触发 panic
#[unsafe(no_mangle)]
pub extern "C" fn TEE_ResetOperation(operation: *mut TEE_OperationHandle) {
    // 检查空句柄
    if operation.is_null() {
        TEE_Panic(TEE_PANIC_ID_TEE_RESETOPERATION);
    }
    let op_handle = unsafe { &*operation };
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_KEY_SET) != 0 {
        TEE_Panic(0);
    }
    // 重置操作状态
    unsafe {
        reset_operation_state(&mut *operation);
    }
}

/// 设置操作的密钥
///
/// # 参数
/// * `operation` - 操作句柄
/// * `key` - 密钥对象句柄
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_SECURITY` - 安全相关错误
///
/// # 注意
/// - 如果操作句柄或密钥句柄为空，函数将触发 panic
/// - 摘要操作不能设置密钥
/// - 操作必须处于初始状态
#[unsafe(no_mangle)]
pub extern "C" fn TEE_SetOperationKey(
    operation: *mut TEE_OperationHandle,
    key: TEE_ObjectHandle,
) -> TEE_Result {
    // 检查空句柄
    if operation.is_null() {
        TEE_Panic(TEE_PANIC_ID_TEE_SETOPERATIONKEY);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取操作句柄
    let op_handle = unsafe { &mut *operation };

    // 检查空密钥句柄
    if key.is_null() {
        // 清除操作密钥
        TEE_ResetTransientObject(op_handle.key1);
        op_handle.info.handleState &= !TEE_HANDLE_FLAG_KEY_SET;

        // 如果操作不在初始状态，重置操作状态
        if op_handle.operation_state != TEE_OPERATION_STATE_INITIAL {
            reset_operation_state(op_handle);
        }
        return TEE_SUCCESS;
    }

    // 检查是否为摘要操作，摘要操作不能设置密钥
    if op_handle.info.operationClass == TEE_OPERATION_DIGEST {
        TEE_Panic(TEE_PANIC_ID_TEE_SETOPERATIONKEY);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查是否期望两个密钥（TEE_ALG_AES_XTS 需要两个密钥）
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_EXPECT_TWO_KEYS) != 0 {
        TEE_Panic(TEE_PANIC_ID_TEE_SETOPERATIONKEY);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取密钥信息
    let mut key_info = TEE_ObjectInfo {
        objectType: 0,
        objectSize: 0,
        maxObjectSize: 0,
        objectUsage: 0,
        dataSize: 0,
        dataPosition: 0,
        handleFlags: 0,
    };

    let res = TEE_GetObjectInfo1(key, &mut key_info);
    if res != TEE_SUCCESS {
        TEE_Panic(TEE_PANIC_ID_TEE_SETOPERATIONKEY);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查提供的密钥是否满足所需的使用要求
    if (key_info.objectUsage & op_handle.info.requiredKeyUsage) != op_handle.info.requiredKeyUsage {
        TEE_Panic(TEE_PANIC_ID_TEE_SETOPERATIONKEY);
        return TEE_ERROR_SECURITY;
    }

    // 检查密钥大小是否超过最大密钥大小
    if op_handle.info.maxKeySize < key_info.objectSize {
        TEE_Panic(TEE_PANIC_ID_TEE_SETOPERATIONKEY);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 重置操作的密钥对象
    TEE_ResetTransientObject(op_handle.key1);
    op_handle.info.handleState &= !TEE_HANDLE_FLAG_KEY_SET;

    // 复制密钥对象属性到操作的密钥对象
    let res = TEE_CopyObjectAttributes1(op_handle.key1, key);
    if res != TEE_SUCCESS {
        TEE_Panic(TEE_PANIC_ID_TEE_SETOPERATIONKEY);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 更新操作状态，标记密钥已设置
    op_handle.info.handleState |= TEE_HANDLE_FLAG_KEY_SET;

    // 更新密钥大小
    op_handle.info.keySize = key_info.objectSize;

    // 如果操作不在初始状态，重置操作状态
    if op_handle.operation_state != TEE_OPERATION_STATE_INITIAL {
        reset_operation_state(op_handle);
    }

    TEE_SUCCESS
}

/// 设置操作的两个密钥
///
/// # 参数
/// * `operation` - 操作句柄
/// * `key1` - 第一个密钥对象句柄
/// * `key2` - 第二个密钥对象句柄
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_SECURITY` - 两个密钥相同
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_CORRUPT_OBJECT` - 密钥对象损坏
/// * `TEE_ERROR_CORRUPT_OBJECT_2` - 第二个密钥对象损坏
/// * `TEE_ERROR_STORAGE_NOT_AVAILABLE` - 存储不可用
/// * `TEE_ERROR_STORAGE_NOT_AVAILABLE_2` - 第二个存储不可用
#[unsafe(no_mangle)]
pub extern "C" fn TEE_SetOperationKey2(
    operation: *mut TEE_OperationHandle,
    key1: TEE_ObjectHandle,
    key2: TEE_ObjectHandle,
) -> TEE_Result {
    // 检查操作句柄是否为空
    if operation.is_null() {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    let op_handle = unsafe { &mut *operation };

    // 检查是否相同的密钥
    if !operation.is_null() && !key1.is_null() && !key2.is_null() && key1 == key2 {
        return TEE_ERROR_SECURITY;
    }

    // 处理两个密钥都为 NULL 的情况
    if key1.is_null() && key2.is_null() {
        // 清除密钥
        TEE_ResetTransientObject(op_handle.key1);
        TEE_ResetTransientObject(op_handle.key2);
        op_handle.info.handleState &= !TEE_HANDLE_FLAG_KEY_SET;
        if op_handle.operation_state != TEE_OPERATION_STATE_INITIAL {
            reset_operation_state(op_handle);
        }
        return TEE_SUCCESS;
    } else if key1.is_null() || key2.is_null() {
        // 处理只有一个密钥为 NULL 的情况
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 摘要操作不能设置密钥
    if op_handle.info.operationClass == TEE_OPERATION_DIGEST {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查是否期望两个密钥
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_EXPECT_TWO_KEYS) == 0 {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取第一个密钥的信息
    let mut key_info1 = TEE_ObjectInfo {
        objectType: 0,
        objectSize: 0,
        maxObjectSize: 0,
        objectUsage: 0,
        dataSize: 0,
        dataPosition: 0,
        handleFlags: 0,
    };

    let mut res = TEE_GetObjectInfo1(key1, &mut key_info1);
    if res != TEE_SUCCESS {
        return handle_result_error(res);
    }

    // 检查第一个密钥是否满足使用要求
    if (key_info1.objectUsage & op_handle.info.requiredKeyUsage) != op_handle.info.requiredKeyUsage
    {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取第二个密钥的信息
    let mut key_info2 = TEE_ObjectInfo {
        objectType: 0,
        objectSize: 0,
        maxObjectSize: 0,
        objectUsage: 0,
        dataSize: 0,
        dataPosition: 0,
        handleFlags: 0,
    };

    res = TEE_GetObjectInfo1(key2, &mut key_info2);
    if res != TEE_SUCCESS {
        return if res == TEE_ERROR_CORRUPT_OBJECT {
            TEE_ERROR_CORRUPT_OBJECT_2
        } else {
            handle_result_error(res)
        };
    }

    // 检查第二个密钥是否满足使用要求
    if (key_info2.objectUsage & op_handle.info.requiredKeyUsage) != op_handle.info.requiredKeyUsage
    {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查两个密钥大小是否相等
    if key_info1.objectSize != key_info2.objectSize {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查密钥大小是否超过最大值
    if op_handle.info.maxKeySize < key_info1.objectSize {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 重置操作的密钥对象
    TEE_ResetTransientObject(op_handle.key1);
    TEE_ResetTransientObject(op_handle.key2);
    op_handle.info.handleState &= !TEE_HANDLE_FLAG_KEY_SET;

    // 复制密钥对象属性到操作的密钥对象
    res = TEE_CopyObjectAttributes1(op_handle.key1, key1);
    if res != TEE_SUCCESS {
        return handle_result_error(res);
    }

    res = TEE_CopyObjectAttributes1(op_handle.key2, key2);
    if res != TEE_SUCCESS {
        return if res == TEE_ERROR_CORRUPT_OBJECT {
            TEE_ERROR_CORRUPT_OBJECT_2
        } else {
            handle_result_error(res)
        };
    }

    // 更新操作状态，标记密钥已设置
    op_handle.info.handleState |= TEE_HANDLE_FLAG_KEY_SET;

    // 更新密钥大小
    op_handle.info.keySize = key_info1.objectSize;

    // 如果操作不在初始状态，重置操作状态
    if op_handle.operation_state != TEE_OPERATION_STATE_INITIAL {
        reset_operation_state(op_handle);
    }

    TEE_SUCCESS
}

/// 处理结果错误
fn handle_result_error(res: TEE_Result) -> TEE_Result {
    match res {
        TEE_SUCCESS => TEE_SUCCESS,
        TEE_ERROR_CORRUPT_OBJECT => TEE_ERROR_CORRUPT_OBJECT,
        TEE_ERROR_CORRUPT_OBJECT_2 => TEE_ERROR_CORRUPT_OBJECT_2,
        TEE_ERROR_STORAGE_NOT_AVAILABLE => TEE_ERROR_STORAGE_NOT_AVAILABLE,
        TEE_ERROR_STORAGE_NOT_AVAILABLE_2 => TEE_ERROR_STORAGE_NOT_AVAILABLE_2,
        _ => {
            TEE_Panic(res as u32);
            res
        }
    }
}

/// 复制操作句柄
///
/// 将源操作句柄的状态复制到目标操作句柄
///
/// # 参数
/// * `dst_op` - 目标操作句柄
/// * `src_op` - 源操作句柄
///
/// # 注意
/// - 如果操作句柄为空，函数将触发 panic
/// - 操作算法和模式必须相同
/// - 目标和源操作的缓冲区配置必须匹配
#[unsafe(no_mangle)]
pub extern "C" fn TEE_CopyOperation(
    dst_op: *mut TEE_OperationHandle,
    src_op: *mut TEE_OperationHandle,
) {
    // 检查空句柄
    if dst_op.is_null() || src_op.is_null() {
        TEE_Panic(0);
    }

    // 安全地解引用指针，使用unsafe块包装解引用操作
    let (dst_ref, src_ref) = unsafe { (&mut *dst_op, &*src_op) };

    // 检查算法和模式是否匹配
    if dst_ref.info.algorithm != src_ref.info.algorithm {
        TEE_Panic(0);
    }
    if dst_ref.info.mode != src_ref.info.mode {
        TEE_Panic(0);
    }

    // 处理非摘要操作的密钥复制
    if src_ref.info.operationClass != TEE_OPERATION_DIGEST {
        let (key1, key2) = if (src_ref.info.handleState & TEE_HANDLE_FLAG_KEY_SET) != 0 {
            (src_ref.key1, src_ref.key2)
        } else {
            (ptr::null_mut(), ptr::null_mut())
        };

        if (src_ref.info.handleState & TEE_HANDLE_FLAG_EXPECT_TWO_KEYS) == 0 {
            TEE_SetOperationKey(dst_op, key1);
        } else {
            // TEE_SetOperationKey2 返回TEE_Result，需要处理返回值
            let res = TEE_SetOperationKey2(dst_op, key1, key2);
            if res != TEE_SUCCESS {
                TEE_Panic(res as u32);
            }
        }
    }

    // 复制操作信息
    dst_ref.info.handleState = src_ref.info.handleState;
    dst_ref.info.keySize = src_ref.info.keySize;
    dst_ref.info.digestLength = src_ref.info.digestLength;
    dst_ref.operation_state = src_ref.operation_state;

    // 检查缓冲区配置是否匹配
    if dst_ref.buffer_two_blocks != src_ref.buffer_two_blocks
        || dst_ref.block_size != src_ref.block_size
    {
        TEE_Panic(0);
    }

    // 复制缓冲区内容
    if !dst_ref.buffer.is_null() {
        if src_ref.buffer.is_null() {
            TEE_Panic(0);
        }

        let sz = if src_ref.buffer_two_blocks {
            src_ref.block_size * 2
        } else {
            src_ref.block_size
        };

        // 使用标准库函数复制缓冲区内容
        unsafe {
            std::ptr::copy_nonoverlapping(src_ref.buffer, dst_ref.buffer, sz);
        }
        dst_ref.buffer_offs = src_ref.buffer_offs;
    } else if !src_ref.buffer.is_null() {
        TEE_Panic(0);
    }

    // 复制加密状态
    let res = unsafe { _utee_cryp_state_copy(dst_ref.state as u64, src_ref.state as u64) };
    if res != TEE_SUCCESS as usize {
        TEE_Panic(res as u32);
    }
}

/// 初始化哈希操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `iv` - 初始化向量（当前实现中未使用）
/// * `iv_len` - 初始化向量长度（当前实现中未使用）
///
/// # 注意
/// IV 和 IVLen 在当前实现中从未使用
/// 因此不检查 IV 和 IVLen 的一致性
fn init_hash_operation(
    operation: *mut TEE_OperationHandle,
    iv: *const core::ffi::c_void,
    iv_len: u32,
) {
    // 检查操作句柄是否为空
    if operation.is_null() {
        TEE_Panic(TEE_ERROR_BAD_PARAMETERS);
        return;
    }

    let res = unsafe { _utee_hash_init((*operation).state as u64, iv, iv_len as usize) };
    if res != TEE_SUCCESS as usize {
        TEE_Panic(res as u32);
    }

    // 重置缓冲区偏移量
    unsafe {
        (*operation).buffer_offs = 0;
        // 设置已初始化标志
        (*operation).info.handleState |= TEE_HANDLE_FLAG_INITIALIZED;
    }
}

/// 更新摘要操作
///
/// 将数据块添加到摘要操作中，更新内部哈希状态
///
/// # 参数
/// * `operation` - 操作句柄，必须是摘要类型的操作
/// * `chunk` - 要处理的数据块指针
/// * `chunk_size` - 数据块大小
///
/// # 安全说明
/// - `operation` 必须是有效的操作句柄
/// - `chunk` 必须指向有效的内存区域，大小至少为 `chunk_size` 字节
/// - `chunk` 指向的数据在函数执行期间不能被其他线程修改
#[unsafe(no_mangle)]
pub extern "C" fn TEE_DigestUpdate(
    operation: *mut TEE_OperationHandle,
    chunk: *const core::ffi::c_void,
    chunk_size: usize,
) {
    // 检查操作句柄是否为空或操作类型是否为摘要操作
    if operation.is_null() {
        TEE_Panic(TEE_ERROR_BAD_PARAMETERS);
        return;
    }

    // 安全地解引用操作句柄
    let op_handle = unsafe { &mut *operation };

    if op_handle.info.operationClass != TEE_OPERATION_DIGEST {
        TEE_Panic(TEE_ERROR_BAD_PARAMETERS);
        return;
    }

    // 设置操作状态为活动状态
    op_handle.operation_state = TEE_OPERATION_STATE_ACTIVE;

    // 调用底层系统调用更新哈希状态
    let res = unsafe { _utee_hash_update(op_handle.state as u64, chunk, chunk_size) };

    if res != TEE_SUCCESS as usize {
        TEE_Panic(res as u32);
    }
}

/// 完成摘要操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `chunk` - 最后一个数据块（可以为 NULL）
/// * `chunk_len` - 最后一个数据块的长度
/// * `hash` - 输出缓冲区，用于存储摘要结果
/// * `hash_len` - 指向缓冲区大小的指针，返回实际摘要长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_SHORT_BUFFER` - 缓冲区太小
#[unsafe(no_mangle)]
pub extern "C" fn TEE_DigestDoFinal(
    operation: *mut TEE_OperationHandle,
    chunk: *const core::ffi::c_void,
    chunk_len: usize,
    hash: *mut core::ffi::c_void,
    hash_len: *mut usize,
) -> TEE_Result {
    // 检查操作句柄
    if operation.is_null() {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查 chunk 和 chunk_len 的一致性
    if chunk.is_null() && chunk_len > 0 {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取操作句柄
    let op_handle = unsafe { &mut *operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_DIGEST {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查操作状态
    if op_handle.operation_state == TEE_OPERATION_STATE_EXTRACTING && chunk_len > 0 {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查输出参数
    if hash_len.is_null() {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查内存访问权限
    if cfg!(feature = "strict_annotation_checks") {
        let res = TEE_CheckMemoryAccessRights(
            TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
            hash_len as *mut core::ffi::c_void,
            std::mem::size_of::<usize>(),
        );
        if res != 0 {
            eprintln!("[inout] hash_len: error {:#010x}", res);
            TEE_Panic(0);
            return TEE_ERROR_BAD_PARAMETERS;
        }
    }

    let res = if op_handle.operation_state == TEE_OPERATION_STATE_EXTRACTING
        && !op_handle.buffer.is_null()
    {
        /*
         * This is not an Extendable-Output Function and we have
         * already started extracting
         */
        let len = std::cmp::min(op_handle.block_size - op_handle.buffer_offs, unsafe {
            *hash_len
        });
        unsafe {
            std::ptr::copy_nonoverlapping(
                op_handle.buffer.add(op_handle.buffer_offs),
                hash as *mut u8,
                len,
            );
            *hash_len = len;
        }
        TEE_SUCCESS
    } else {
        let mut hl = unsafe { *hash_len as u64 };
        let res =
            unsafe { _utee_hash_final(op_handle.state as u64, chunk, chunk_len, hash, &mut hl) };
        unsafe {
            *hash_len = hl as usize;
        }

        if res != TEE_SUCCESS as usize {
            return res as TEE_Result;
        }

        TEE_SUCCESS
    };

    // 重置操作状态
    init_hash_operation(operation, core::ptr::null(), 0);

    // 设置操作状态为初始状态
    unsafe {
        (*operation).operation_state = TEE_OPERATION_STATE_INITIAL;
    }

    // 检查返回结果
    if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
        TEE_Panic(res as u32);
    }

    res
}

/// 提取摘要操作结果
///
/// # 参数
/// * `operation` - 操作句柄
/// * `hash` - 输出缓冲区，用于存储摘要结果
/// * `hash_len` - 指向缓冲区大小的指针，返回实际摘要长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
#[unsafe(no_mangle)]
pub extern "C" fn TEE_DigestExtract(
    operation: *mut TEE_OperationHandle,
    hash: *mut core::ffi::c_void,
    hash_len: *mut usize,
) -> TEE_Result {
    // 检查操作句柄
    if operation.is_null() {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取操作句柄
    let op_handle = unsafe { &mut *operation };
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_KEY_SET) == 0 {
        TEE_Panic(0);
    }
    // 重置操作状态
    unsafe {
        reset_operation_state(&mut *operation);
    }

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_DIGEST {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查输出参数
    if hash_len.is_null() {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查内存访问权限
    if cfg!(feature = "strict_annotation_checks") {
        let res = TEE_CheckMemoryAccessRights(
            TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
            hash_len as *mut core::ffi::c_void,
            std::mem::size_of::<usize>(),
        );
        if res != 0 {
            eprintln!("[inout] hash_len: error {:#010x}", res);
            TEE_Panic(0);
            return TEE_ERROR_BAD_PARAMETERS;
        }
    }

    // 检查是否有缓冲区
    if op_handle.buffer.is_null() {
        /* This is an Extendable-Output Function */
        unsafe {
            (*operation).info.handleState |= TEE_HANDLE_FLAG_EXTRACTING;
            (*operation).operation_state = TEE_OPERATION_STATE_EXTRACTING;
        }

        let mut hl = unsafe { *hash_len as u64 };
        let res = unsafe {
            _utee_hash_final(op_handle.state as u64, core::ptr::null(), 0, hash, &mut hl)
        };
        if res != TEE_SUCCESS as usize {
            TEE_Panic(0);
            return TEE_ERROR_BAD_PARAMETERS;
        }

        unsafe {
            *hash_len = hl as usize;
        }

        return TEE_SUCCESS;
    }

    // 检查操作状态
    if op_handle.operation_state != TEE_OPERATION_STATE_EXTRACTING {
        let mut hl = op_handle.block_size as u64;
        let res = unsafe {
            _utee_hash_final(
                op_handle.state as u64,
                core::ptr::null(),
                0,
                op_handle.buffer as *mut core::ffi::c_void,
                &mut hl,
            )
        };
        if res != TEE_SUCCESS as usize {
            TEE_Panic(0);
            return TEE_ERROR_BAD_PARAMETERS;
        }

        if hl as usize != op_handle.block_size {
            TEE_Panic(0);
            return TEE_ERROR_BAD_PARAMETERS;
        }

        debug_assert!(op_handle.buffer_offs == 0, "buffer_offs should be 0");

        unsafe {
            (*operation).info.handleState |= TEE_HANDLE_FLAG_EXTRACTING;
            (*operation).operation_state = TEE_OPERATION_STATE_EXTRACTING;
        }
    }

    // 计算复制长度
    let len = std::cmp::min(op_handle.block_size - op_handle.buffer_offs, unsafe {
        *hash_len
    });

    // 复制数据到输出缓冲区
    unsafe {
        std::ptr::copy_nonoverlapping(
            op_handle.buffer.add(op_handle.buffer_offs),
            hash as *mut u8,
            len,
        );
        *hash_len = len;
    }

    // 更新缓冲区偏移量
    unsafe {
        (*operation).buffer_offs += len;
    }

    TEE_SUCCESS
}

/// 初始化加密操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `iv` - 初始化向量（IV）
/// * `iv_len` - 初始化向量长度
///
/// # 注意
/// - ECB 模式的加密算法不能使用初始化向量
/// - 操作必须已设置密钥
#[unsafe(no_mangle)]
pub extern "C" fn TEE_CipherInit(
    operation: *mut TEE_OperationHandle,
    iv: *const core::ffi::c_void,
    iv_len: usize,
) {
    use crate::tee_api_defines::*;

    // 检查操作句柄
    if operation.is_null() {
        TEE_Panic(0);
        return;
    }

    // 获取操作句柄
    let op_handle = unsafe { &mut *operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_CIPHER {
        TEE_Panic(0);
        return;
    }

    // 检查密钥是否已设置
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_KEY_SET) == 0 || op_handle.key1.is_null() {
        TEE_Panic(0);
        return;
    }

    // 如果操作不在初始状态，重置操作
    if op_handle.operation_state != TEE_OPERATION_STATE_INITIAL {
        TEE_ResetOperation(operation);
    }

    // 检查 ECB 模式算法是否使用了初始化向量
    if !iv.is_null() && iv_len > 0 {
        match op_handle.info.algorithm {
            TEE_ALG_AES_ECB_NOPAD
            | TEE_ALG_DES_ECB_NOPAD
            | TEE_ALG_DES3_ECB_NOPAD
            | TEE_ALG_SM4_ECB_NOPAD => {
                TEE_Panic(0);
                return;
            }
            _ => {}
        }
    }

    // 设置操作状态为活动状态
    unsafe {
        (*operation).operation_state = TEE_OPERATION_STATE_ACTIVE;
    }

    // 初始化加密操作
    let res = unsafe { _utee_cipher_init(op_handle.state as u64, iv, iv_len) };
    if res != TEE_SUCCESS as usize {
        TEE_Panic(res as u32);
        return;
    }

    // 重置缓冲区偏移量
    unsafe {
        (*operation).buffer_offs = 0;
        (*operation).info.handleState |= TEE_HANDLE_FLAG_INITIALIZED;
    }
}

/// 向上舍入到最近的 2 的倍数
const fn roundup2(v: usize, r: usize) -> usize {
    (v + r - 1) & !(r - 1)
}

/// 更新函数类型
type UpdateFunc = unsafe extern "C" fn(
    state: u64,
    src: *const core::ffi::c_void,
    slen: usize,
    dst: *mut core::ffi::c_void,
    dlen: *mut u64,
) -> usize;

/// 缓冲区更新函数
///
/// 此函数管理加密/解密操作的缓冲区更新，包括：
/// - 填充缓冲区以形成完整的块
/// - 处理缓冲区中的数据
/// - 从源数据直接处理尽可能多的数据
/// - 将剩余数据存入缓冲区
///
/// # 参数
/// * `op` - 操作句柄
/// * `update_func` - 更新函数指针，用于处理数据
/// * `src_data` - 源数据指针
/// * `src_len` - 源数据长度
/// * `dest_data` - 目标缓冲区指针
/// * `dest_len` - 指向目标缓冲区长度的指针，返回实际写入的数据长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
fn tee_buffer_update(
    op: &mut TEE_OperationHandle,
    update_func: UpdateFunc,
    src_data: *const core::ffi::c_void,
    mut src_len: usize,
    dest_data: *mut core::ffi::c_void,
    dest_len: *mut u64,
) -> TEE_Result {
    use crate::tee_api_defines::*;

    // 检查源数据指针
    if src_data.is_null() {
        if src_len > 0 {
            TEE_Panic(0);
        }
        unsafe {
            *dest_len = 0;
        }
        return TEE_SUCCESS;
    }

    let mut src = src_data as *const u8;
    let mut dst = dest_data as *mut u8;
    let mut dlen = unsafe { *dest_len } as usize;
    let mut acc_dlen = 0usize;

    // 确定缓冲区大小
    let (buffer_size, buffer_left) = if op.buffer_two_blocks {
        (op.block_size * 2, 1usize)
    } else {
        (op.block_size, 0usize)
    };

    // 填充缓冲区以形成完整的块
    if op.buffer_offs > 0 {
        let l = if op.buffer_offs < op.block_size {
            std::cmp::min(src_len, op.block_size - op.buffer_offs)
        } else {
            std::cmp::min(src_len, buffer_size - op.buffer_offs)
        };

        unsafe {
            std::ptr::copy_nonoverlapping(src, op.buffer.add(op.buffer_offs), l);
        }
        op.buffer_offs += l;
        src = unsafe { src.add(l) };
        src_len -= l;

        // 如果缓冲区偏移量不是块大小的倍数，则无法继续处理
        if op.buffer_offs % op.block_size != 0 {
            unsafe {
                *dest_len = acc_dlen as u64;
            }
            return TEE_SUCCESS;
        }
    }

    // 如果可以从缓冲区提供数据
    if op.buffer_offs > 0 && op.buffer_offs + src_len >= buffer_size + buffer_left {
        let mut l = roundup2(op.buffer_offs + src_len - buffer_size, op.block_size);
        l = std::cmp::min(op.buffer_offs, l);

        // 如果只缓冲单个块，立即处理它
        if !op.buffer_two_blocks {
            l = op.block_size;
        }

        let mut tmp_dlen = dlen as u64;
        let res = unsafe {
            update_func(
                op.state as u64,
                op.buffer as *const core::ffi::c_void,
                l,
                dst as *mut core::ffi::c_void,
                &mut tmp_dlen,
            )
        };

        if res != TEE_SUCCESS as usize {
            TEE_Panic(res as u32);
        }

        let tmp_dlen = tmp_dlen as usize;
        unsafe {
            dst = dst.add(tmp_dlen);
        }
        dlen -= tmp_dlen;
        acc_dlen += tmp_dlen;
        op.buffer_offs -= l;

        // 如果缓冲区中仍有数据
        if op.buffer_offs > 0 {
            // 源数据长度足够小，可以包含在剩余缓冲区中
            unsafe {
                std::ptr::copy(op.buffer.add(l), op.buffer, buffer_size - l);
                std::ptr::copy_nonoverlapping(src, op.buffer.add(op.buffer_offs), src_len);
            }
            op.buffer_offs += src_len;

            unsafe {
                *dest_len = acc_dlen as u64;
            }
            return TEE_SUCCESS;
        }
    }

    // 如果源数据长度足够大，直接处理
    if src_len >= buffer_size + buffer_left {
        // 缓冲区为空，尽可能从源数据中提供数据
        let l = if op.buffer_two_blocks {
            roundup2(src_len - buffer_size, op.block_size)
        } else {
            roundup2(src_len - buffer_size + 1, op.block_size)
        };

        let mut tmp_dlen = dlen as u64;
        let res = unsafe {
            update_func(
                op.state as u64,
                src as *const core::ffi::c_void,
                l,
                dst as *mut core::ffi::c_void,
                &mut tmp_dlen,
            )
        };

        if res != TEE_SUCCESS as usize {
            TEE_Panic(res as u32);
        }

        let tmp_dlen = tmp_dlen as usize;
        unsafe {
            src = src.add(l);
        }
        src_len -= l;
        acc_dlen += tmp_dlen;
    }

    // 源数据长度足够小，可以包含在缓冲区中
    unsafe {
        std::ptr::copy_nonoverlapping(src, op.buffer.add(op.buffer_offs), src_len);
    }
    op.buffer_offs += src_len;

    unsafe {
        *dest_len = acc_dlen as u64;
    }
    TEE_SUCCESS
}

/// 更新加密操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `src_data` - 源数据指针
/// * `src_len` - 源数据长度
/// * `dest_data` - 目标缓冲区指针
/// * `dest_len` - 指向目标缓冲区长度的指针，返回实际写入的数据长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_SHORT_BUFFER` - 缓冲区太小
#[unsafe(no_mangle)]
pub extern "C" fn TEE_CipherUpdate(
    operation: *mut TEE_OperationHandle,
    src_data: *const core::ffi::c_void,
    src_len: usize,
    dest_data: *mut core::ffi::c_void,
    dest_len: *mut usize,
) -> TEE_Result {
    // 检查操作句柄和源数据
    if operation.is_null() || (src_data.is_null() && src_len > 0) {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查输出参数的内存访问权限
    if cfg!(feature = "strict_annotation_checks") {
        let res = TEE_CheckMemoryAccessRights(
            TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
            dest_len as *mut core::ffi::c_void,
            std::mem::size_of::<usize>(),
        );
        if res != 0 {
            eprintln!("[inout] destLen: error {:#010x}", res);
            return TEE_ERROR_BAD_PARAMETERS;
        }
    }

    // 获取操作句柄的可变引用
    let op_handle = unsafe { &mut *operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_CIPHER {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查是否已初始化
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_INITIALIZED) == 0 {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查操作状态
    if op_handle.operation_state != TEE_OPERATION_STATE_ACTIVE {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 处理空数据情况
    if src_data.is_null() && src_len == 0 {
        unsafe {
            *dest_len = 0;
        }
        return TEE_SUCCESS;
    }

    // 计算所需的目标缓冲区长度
    let req_dlen = if op_handle.block_size > 1 {
        let base =
            ((op_handle.buffer_offs + src_len) / op_handle.block_size) * op_handle.block_size;
        if op_handle.buffer_two_blocks {
            if op_handle.buffer_offs + src_len > op_handle.block_size * 2 {
                let req = op_handle.buffer_offs + src_len - op_handle.block_size * 2;
                roundup2(req, op_handle.block_size)
            } else {
                0
            }
        } else {
            base
        }
    } else {
        src_len
    };

    /*
     * 在开始将数据提供给算法之前，检查目标缓冲区是否足够大。
     * 提供数据期间的错误是致命的，因为我们无法使用此 API 恢复同步。
     */
    unsafe {
        if *dest_len < req_dlen {
            *dest_len = req_dlen;
            let res = TEE_ERROR_SHORT_BUFFER;
            if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
                TEE_Panic(res as u32);
            }
            return res;
        }
    }

    let mut dl = unsafe { *dest_len } as u64;
    let res = if op_handle.block_size > 1 {
        tee_buffer_update(
            op_handle,
            _utee_cipher_update,
            src_data,
            src_len,
            dest_data,
            &mut dl,
        )
    } else {
        if src_len > 0 {
            unsafe {
                _utee_cipher_update(
                    op_handle.state as u64,
                    src_data,
                    src_len,
                    dest_data,
                    &mut dl,
                ) as TEE_Result
            }
        } else {
            dl = 0;
            TEE_SUCCESS
        }
    };

    unsafe {
        *dest_len = dl as usize;
    }

    if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
        TEE_Panic(res as u32);
    }

    res
}

/// 完成加密操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `src_data` - 源数据指针
/// * `src_len` - 源数据长度
/// * `dest_data` - 目标缓冲区指针
/// * `dest_len` - 指向目标缓冲区长度的指针，返回实际写入的数据长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_SHORT_BUFFER` - 缓冲区太小
#[unsafe(no_mangle)]
pub extern "C" fn TEE_CipherDoFinal(
    operation: *mut TEE_OperationHandle,
    src_data: *const core::ffi::c_void,
    src_len: usize,
    dest_data: *mut core::ffi::c_void,
    dest_len: *mut usize,
) -> TEE_Result {
    // 检查操作句柄和源数据
    if operation.is_null() || (src_data.is_null() && src_len > 0) {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查输出参数的内存访问权限
    if !dest_len.is_null() {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
                dest_len as *mut core::ffi::c_void,
                std::mem::size_of::<usize>(),
            );
            if check_res != 0 {
                eprintln!("[inout] destLen: error {:#010x}", check_res);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    }

    // 获取操作句柄
    let op_handle = unsafe { &mut *operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_CIPHER {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查是否已初始化
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_INITIALIZED) == 0 {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查操作状态
    if op_handle.operation_state != TEE_OPERATION_STATE_ACTIVE {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    /*
     * 检查对于需要客户端提供填充的算法，最后一个块是否需要填充。
     */
    match op_handle.info.algorithm {
        TEE_ALG_AES_ECB_NOPAD
        | TEE_ALG_AES_CBC_NOPAD
        | TEE_ALG_DES_ECB_NOPAD
        | TEE_ALG_DES_CBC_NOPAD
        | TEE_ALG_DES3_ECB_NOPAD
        | TEE_ALG_DES3_CBC_NOPAD
        | TEE_ALG_SM4_ECB_NOPAD
        | TEE_ALG_SM4_CBC_NOPAD => {
            if (op_handle.buffer_offs + src_len) % op_handle.block_size != 0 {
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
        _ => {}
    }

    /*
     * 在开始将数据提供给算法之前，检查目标缓冲区是否足够大。
     * 提供数据期间的错误是致命的，因为我们无法使用此 API 恢复同步。
     */
    let req_dlen = if op_handle.block_size > 1 {
        op_handle.buffer_offs + src_len
    } else {
        src_len
    };

    let mut tmp_dlen = if !dest_len.is_null() {
        unsafe { *dest_len as u64 }
    } else {
        0u64
    };

    if tmp_dlen < req_dlen as u64 {
        if !dest_len.is_null() {
            unsafe {
                *dest_len = req_dlen;
            }
        }
        let res = TEE_ERROR_SHORT_BUFFER;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    let mut res = TEE_SUCCESS;
    let mut dst = dest_data as *mut u8;
    let mut acc_dlen = 0usize;

    if op_handle.block_size > 1 {
        if src_len > 0 {
            res = tee_buffer_update(
                unsafe { &mut *operation },
                _utee_cipher_update,
                src_data,
                src_len,
                dest_data,
                &mut tmp_dlen,
            );

            if res != TEE_SUCCESS {
                if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
                    TEE_Panic(res as u32);
                }
                return res;
            }

            let tmp_dlen_usize = tmp_dlen as usize;
            unsafe {
                dst = dst.add(tmp_dlen_usize);
            }
            acc_dlen += tmp_dlen_usize;

            if !dest_len.is_null() {
                unsafe {
                    tmp_dlen = *dest_len as u64 - acc_dlen as u64;
                }
            }
        }

        res = unsafe {
            _utee_cipher_final(
                op_handle.state as u64,
                op_handle.buffer as *const core::ffi::c_void,
                op_handle.buffer_offs,
                dst as *mut core::ffi::c_void,
                &mut tmp_dlen,
            ) as TEE_Result
        };
    } else {
        res = unsafe {
            _utee_cipher_final(
                op_handle.state as u64,
                src_data,
                src_len,
                dst as *mut core::ffi::c_void,
                &mut tmp_dlen,
            ) as TEE_Result
        };
    }

    if res != TEE_SUCCESS {
        if res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    acc_dlen += tmp_dlen as usize;
    if !dest_len.is_null() {
        unsafe {
            *dest_len = acc_dlen;
        }
    }

    // 清除已初始化标志
    unsafe {
        (*operation).info.handleState &= !TEE_HANDLE_FLAG_INITIALIZED;
        (*operation).operation_state = TEE_OPERATION_STATE_INITIAL;
    }

    res
}

/// 初始化 MAC 操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `iv` - 初始化向量（IV）
/// * `iv_len` - 初始化向量长度
///
/// # 注意
/// - 操作必须是 MAC 类型
/// - 操作必须已设置密钥
/// - 如果操作不在初始状态，会先重置操作
#[unsafe(no_mangle)]
pub extern "C" fn TEE_MACInit(
    operation: *mut TEE_OperationHandle,
    iv: *const core::ffi::c_void,
    iv_len: usize,
) {
    // 检查操作句柄是否为空
    if operation.is_null() {
        TEE_Panic(TEE_PANIC_ID_TEE_MACINIT);
        return;
    }

    // 安全地解引用操作句柄（第一次 unsafe：仅用于验证）
    let op_handle = unsafe { &*operation };

    // 检查操作类型是否为 MAC
    if op_handle.info.operationClass != TEE_OPERATION_MAC {
        TEE_Panic(TEE_PANIC_ID_TEE_MACINIT);
        return;
    }

    // 检查密钥是否已设置
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_KEY_SET) == 0 || op_handle.key1.is_null() {
        TEE_Panic(TEE_PANIC_ID_TEE_MACINIT);
        return;
    }

    // 如果操作不在初始状态，重置操作
    if op_handle.operation_state != TEE_OPERATION_STATE_INITIAL {
        TEE_ResetOperation(operation);
    }

    // 设置操作状态为活动状态
    unsafe {
        (*operation).operation_state = TEE_OPERATION_STATE_ACTIVE;
    }

    // 调用哈希初始化函数（这会设置 INITIALIZED 标志和重置缓冲区偏移）
    init_hash_operation(operation, iv, iv_len as u32);
}

/// 更新 MAC 操作
///
/// 将数据块添加到 MAC 操作中，更新内部哈希状态
///
/// # 参数
/// * `operation` - 操作句柄，必须是 MAC 类型的操作
/// * `chunk` - 要处理的数据块指针
/// * `chunk_size` - 数据块大小
///
/// # 安全说明
/// - `operation` 必须是有效的操作句柄
/// - `chunk` 必须指向有效的内存区域，大小至少为 `chunk_size` 字节
/// - `chunk` 指向的数据在函数执行期间不能被其他线程修改
/// - 操作必须已通过 TEE_MACInit 初始化
#[unsafe(no_mangle)]
pub extern "C" fn TEE_MACUpdate(
    operation: *mut TEE_OperationHandle,
    chunk: *const core::ffi::c_void,
    chunk_size: usize,
) {
    // 检查操作句柄是否为空
    if operation.is_null() {
        TEE_Panic(TEE_PANIC_ID_TEE_MACINIT);
        return;
    }

    // 检查 chunk 和 chunk_size 的一致性
    if chunk.is_null() && chunk_size > 0 {
        TEE_Panic(TEE_PANIC_ID_TEE_MACINIT);
        return;
    }

    // 安全地解引用操作句柄（第一次 unsafe：仅用于验证）
    let op_handle = unsafe { &*operation };

    // 检查操作类型是否为 MAC
    if op_handle.info.operationClass != TEE_OPERATION_MAC {
        TEE_Panic(TEE_PANIC_ID_TEE_MACINIT);
        return;
    }

    // 检查操作是否已初始化
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_INITIALIZED) == 0 {
        TEE_Panic(TEE_PANIC_ID_TEE_MACINIT);
        return;
    }

    // 检查操作状态是否为活动状态
    if op_handle.operation_state != TEE_OPERATION_STATE_ACTIVE {
        TEE_Panic(TEE_PANIC_ID_TEE_MACINIT);
        return;
    }

    // 调用底层系统调用更新哈希状态（第二次 unsafe：仅用于 FFI 调用）
    let res = unsafe { _utee_hash_update(op_handle.state as u64, chunk, chunk_size) };

    if res != TEE_SUCCESS as usize {
        TEE_Panic(res as u32);
    }
}

/// 完成 MAC 操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `message` - 要处理的消息数据指针
/// * `message_len` - 消息数据长度
/// * `mac` - 输出缓冲区，用于存储 MAC 结果
/// * `mac_len` - 指向输出缓冲区大小的指针，返回实际 MAC 长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_SHORT_BUFFER` - 缓冲区太小
#[unsafe(no_mangle)]
pub extern "C" fn TEE_MACComputeFinal(
    operation: *mut TEE_OperationHandle,
    message: *const core::ffi::c_void,
    message_len: usize,
    mac: *mut core::ffi::c_void,
    mac_len: *mut usize,
) -> TEE_Result {
    // 检查操作句柄和消息参数的一致性
    if operation.is_null()
        || (!message.is_null() && message_len == 0)
        || (message.is_null() && message_len > 0)
    {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查输出参数的内存访问权限
    if cfg!(feature = "strict_annotation_checks") {
        let res = TEE_CheckMemoryAccessRights(
            TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
            mac_len as *mut core::ffi::c_void,
            std::mem::size_of::<usize>(),
        );
        if res != 0 {
            eprintln!("[inout] mac_len: error {:#010x}", res);
            TEE_Panic(0);
            return TEE_ERROR_BAD_PARAMETERS;
        }
    }

    // 获取操作句柄
    let op_handle = unsafe { &mut *operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_MAC {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查是否已初始化
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_INITIALIZED) == 0 {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查操作状态
    if op_handle.operation_state != TEE_OPERATION_STATE_ACTIVE {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取期望的 MAC 长度
    let mut ml = unsafe { *mac_len as u64 };

    // 调用底层系统调用完成 MAC 计算
    let res =
        unsafe { _utee_hash_final(op_handle.state as u64, message, message_len, mac, &mut ml) };

    // 更新实际 MAC 长度
    unsafe {
        *mac_len = ml as usize;
    }

    if res != TEE_SUCCESS as usize {
        let result = res as TEE_Result;
        if result != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(result as u32);
        }
        return result;
    }

    // 清除已初始化标志
    unsafe {
        (*operation).info.handleState &= !TEE_HANDLE_FLAG_INITIALIZED;
        (*operation).operation_state = TEE_OPERATION_STATE_INITIAL;
    }

    TEE_SUCCESS as TEE_Result
}

/// 比较 MAC 操作的最终结果
///
/// # 参数
/// * `operation` - 操作句柄
/// * `message` - 要处理的消息数据指针
/// * `message_len` - 消息数据长度
/// * `mac` - 提供的 MAC 值指针
/// * `mac_len` - 提供的 MAC 长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_MAC_INVALID` - MAC 验证失败
#[unsafe(no_mangle)]
pub extern "C" fn TEE_MACCompareFinal(
    operation: *mut TEE_OperationHandle,
    message: *const core::ffi::c_void,
    message_len: usize,
    mac: *const core::ffi::c_void,
    mac_len: usize,
) -> TEE_Result {
    // 检查操作句柄
    if operation.is_null() {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取操作句柄
    let op_handle = unsafe { &mut *operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_MAC {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查是否已初始化
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_INITIALIZED) == 0 {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查操作状态
    if op_handle.operation_state != TEE_OPERATION_STATE_ACTIVE {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 创建计算的 MAC 缓冲区
    let mut computed_mac = [0u8; TEE_MAX_HASH_SIZE as usize];
    let mut computed_mac_size = TEE_MAX_HASH_SIZE as usize;

    // 调用 TEE_MACComputeFinal 计算 MAC
    let res = TEE_MACComputeFinal(
        operation,
        message,
        message_len,
        computed_mac.as_mut_ptr() as *mut core::ffi::c_void,
        &mut computed_mac_size,
    );

    if res != TEE_SUCCESS {
        if res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 比较 MAC 长度
    if computed_mac_size != mac_len {
        return TEE_ERROR_MAC_INVALID;
    }

    // 比较 MAC 值
    let provided_mac = unsafe { std::slice::from_raw_parts(mac as *const u8, mac_len) };

    if !consttime_memcmp(provided_mac, &computed_mac[..computed_mac_size]) {
        return TEE_ERROR_MAC_INVALID;
    }

    // 设置操作状态为初始状态
    unsafe {
        (*operation).operation_state = TEE_OPERATION_STATE_INITIAL;
    }

    TEE_SUCCESS
}

/// 恒定时间内存比较函数
///
/// # 参数
/// * `a` - 第一个缓冲区
/// * `b` - 第二个缓冲区
/// * `len` - 比较的字节数
///
/// # 返回值
/// * `true` - 如果两个缓冲区相等
/// * `false` - 如果两个缓冲区不相等
fn consttime_memcmp(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// 初始化 AE (Authenticated Encryption) 操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `nonce` - 随机数/初始化向量指针
/// * `nonce_len` - 随机数长度
/// * `tag_len` - 认证标签长度（以位为单位）
/// * `aad_len` - 附加认证数据长度
/// * `payload_len` - 载荷数据长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_NOT_SUPPORTED` - 不支持的操作
#[unsafe(no_mangle)]
pub extern "C" fn TEE_AEInit(
    operation: *mut TEE_OperationHandle,
    nonce: *const core::ffi::c_void,
    nonce_len: usize,
    tag_len: u32,
    aad_len: usize,
    payload_len: usize,
) -> TEE_Result {
    // 检查操作句柄和随机数参数
    if operation.is_null() || nonce.is_null() {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取操作句柄
    let op_handle = unsafe { &mut *operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_AE {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查操作状态
    if op_handle.operation_state != TEE_OPERATION_STATE_INITIAL {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查 AES-GCM 算法的标签长度
    if op_handle.info.algorithm == TEE_ALG_AES_GCM {
        // 从 GP 规范：对于 AES-GCM，可以是 128, 120, 112, 104 或 96
        if tag_len < 96 || tag_len > 128 || (tag_len % 8 != 0) {
            return TEE_ERROR_NOT_SUPPORTED;
        }
    }

    // 调用底层认证加密初始化
    let res = unsafe {
        _utee_authenc_init(
            op_handle.state as u64,
            nonce,
            nonce_len,
            tag_len as usize / 8, // 转换为字节
            aad_len,
            payload_len,
        )
    };

    if res != TEE_SUCCESS as usize {
        let result = res as TEE_Result;
        if result != TEE_ERROR_NOT_SUPPORTED {
            TEE_Panic(result as u32);
        }
        return result;
    }

    // 更新操作信息
    unsafe {
        (*operation).info.digestLength = (tag_len / 8) as u32; // 转换为字节
        (*operation).buffer_offs = 0;
        (*operation).info.handleState |= TEE_HANDLE_FLAG_INITIALIZED;
    }

    TEE_SUCCESS
}

/// 更新 AE (Authenticated Encryption) 操作的附加认证数据 (AAD)
///
/// # 参数
/// * `operation` - 操作句柄
/// * `aad_data` - AAD 数据指针
/// * `aad_data_len` - AAD 数据长度
#[unsafe(no_mangle)]
pub extern "C" fn TEE_AEUpdateAAD(
    operation: *mut TEE_OperationHandle,
    aad_data: *const core::ffi::c_void,
    aad_data_len: usize,
) {
    // 检查操作句柄和 AAD 参数
    if operation.is_null() || (aad_data.is_null() && aad_data_len > 0) {
        TEE_Panic(0);
        return;
    }

    // 获取操作句柄
    let op_handle = unsafe { &*operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_AE {
        TEE_Panic(0);
        return;
    }

    // 检查操作状态
    if op_handle.operation_state != TEE_OPERATION_STATE_INITIAL {
        TEE_Panic(0);
        return;
    }

    // 检查操作是否已初始化
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_INITIALIZED) == 0 {
        TEE_Panic(0);
        return;
    }

    // 调用底层系统调用更新 AAD
    let res = unsafe { _utee_authenc_update_aad(op_handle.state as u64, aad_data, aad_data_len) };

    if res != TEE_SUCCESS as usize {
        TEE_Panic(res as u32);
    }
}

/// AE (Authenticated Encryption) 操作的更新辅助函数
///
/// # 参数
/// * `operation` - 操作句柄
/// * `src` - 源数据指针
/// * `slen` - 源数据长度
/// * `dst` - 目标缓冲区指针
/// * `dlen` - 指向目标缓冲区大小的指针，返回实际写入的数据长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_SHORT_BUFFER` - 目标缓冲区太小
fn ae_update_helper(
    operation: *mut TEE_OperationHandle,
    src: *const core::ffi::c_void,
    slen: usize,
    dst: *mut core::ffi::c_void,
    dlen: *mut usize,
) -> TEE_Result {
    use crate::tee_api_defines::*;

    // 检查空指针参数
    if src.is_null() && slen == 0 {
        unsafe {
            *dlen = 0;
        }
        return TEE_SUCCESS;
    }

    // 检查操作句柄
    if operation.is_null() {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取操作句柄
    let op_handle = unsafe { &*operation };

    // 检查目标长度参数
    if dlen.is_null() {
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 计算所需的缓冲区大小
    let req_dlen = if op_handle.block_size > 1 {
        // 计算需要处理的数据块大小
        let total_size = op_handle.buffer_offs + slen;
        // 使用roundup2函数向上舍入到最近的块大小的倍数
        roundup2(total_size, op_handle.block_size)
    } else {
        slen
    };

    // 检查目标缓冲区是否足够大
    let provided_dlen = unsafe { *dlen };
    if provided_dlen < req_dlen {
        unsafe {
            *dlen = req_dlen;
        }
        return TEE_ERROR_SHORT_BUFFER;
    }

    // 调用底层更新函数
    let mut dl = provided_dlen as u64;
    let res = if op_handle.block_size > 1 {
        // 使用缓冲区更新函数处理块数据
        tee_buffer_update(
            unsafe { &mut *operation },
            _utee_authenc_update_payload,
            src,
            slen,
            dst,
            &mut dl,
        )
    } else {
        // 直接处理非块数据
        if slen > 0 {
            unsafe {
                _utee_authenc_update_payload(op_handle.state as u64, src, slen, dst, &mut dl)
                    as TEE_Result
            }
        } else {
            dl = 0;
            TEE_SUCCESS
        }
    };

    // 更新实际写入的长度
    if res == TEE_SUCCESS {
        unsafe {
            *dlen = dl as usize;
        }
    }

    res
}

/// 更新 AE (Authenticated Encryption) 操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `src_data` - 源数据指针
/// * `src_len` - 源数据长度
/// * `dest_data` - 目标缓冲区指针
/// * `dest_len` - 指向目标缓冲区大小的指针，返回实际写入的数据长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_SHORT_BUFFER` - 目标缓冲区太小
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
#[unsafe(no_mangle)]
pub extern "C" fn TEE_AEUpdate(
    operation: *mut TEE_OperationHandle,
    src_data: *const core::ffi::c_void,
    src_len: usize,
    dest_data: *mut core::ffi::c_void,
    dest_len: *mut usize,
) -> TEE_Result {
    // 检查操作句柄和源数据参数
    if operation.is_null() || (src_data.is_null() && src_len > 0) {
        let res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 检查输出参数的内存访问权限
    if !dest_data.is_null() && !dest_len.is_null() {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
                dest_len as *mut core::ffi::c_void,
                std::mem::size_of::<usize>(),
            );
            if check_res != 0 {
                eprintln!("[inout] destLen: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    }

    // 获取操作句柄
    let op_handle = unsafe { &*operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_AE {
        let res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 检查操作是否已初始化
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_INITIALIZED) == 0 {
        let res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 调用 AE 更新辅助函数
    let res = ae_update_helper(operation, src_data, src_len, dest_data, dest_len);
    if res != TEE_ERROR_SHORT_BUFFER && src_len > 0 {
        unsafe {
            (*operation).operation_state = TEE_OPERATION_STATE_ACTIVE;
        }
    }

    // 检查返回结果
    if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
        TEE_Panic(res as u32);
    }

    res
}

/// AE (Authenticated Encryption) 加密最终操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `src_data` - 源数据指针
/// * `src_len` - 源数据长度
/// * `dest_data` - 目标缓冲区指针
/// * `dest_len` - 指向目标缓冲区大小的指针，返回实际写入的数据长度
/// * `tag` - 认证标签缓冲区指针
/// * `tag_len` - 指向标签缓冲区大小的指针，返回实际标签长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_SHORT_BUFFER` - 缓冲区太小
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
#[unsafe(no_mangle)]
pub extern "C" fn TEE_AEEncryptFinal(
    operation: *mut TEE_OperationHandle,
    src_data: *const core::ffi::c_void,
    src_len: usize,
    dest_data: *mut core::ffi::c_void,
    dest_len: *mut usize,
    tag: *mut core::ffi::c_void,
    tag_len: *mut usize,
) -> TEE_Result {
    let mut res = TEE_SUCCESS;

    // 检查操作句柄和源数据参数
    if operation.is_null() || (src_data.is_null() && src_len > 0) {
        res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 检查输出参数的内存访问权限
    if !dest_len.is_null() {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
                dest_len as *mut core::ffi::c_void,
                std::mem::size_of::<usize>(),
            );
            if check_res != 0 {
                eprintln!("[inout] destLen: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    }

    if !tag_len.is_null() {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
                tag_len as *mut core::ffi::c_void,
                std::mem::size_of::<usize>(),
            );
            if check_res != 0 {
                eprintln!("[inout] tagLen: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    }

    // 获取操作句柄
    let op_handle = unsafe { &*operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_AE {
        let res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 检查操作是否已初始化
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_INITIALIZED) == 0 {
        let res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 检查dest_len和tag_len是否为NULL
    if dest_len.is_null() || tag_len.is_null() {
        res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 获取当前参数值
    let dest_len_val = unsafe { *dest_len };
    let tag_len_val = unsafe { *tag_len };

    // 计算所需的缓冲区大小
    let req_dlen = op_handle.buffer_offs + src_len;

    // 检查目标缓冲区是否足够大
    if dest_len_val < req_dlen {
        unsafe {
            *dest_len = req_dlen;
        }
        res = TEE_ERROR_SHORT_BUFFER;
    }

    // 检查标签缓冲区是否足够大
    if tag_len_val < op_handle.info.digestLength as usize {
        unsafe {
            *tag_len = op_handle.info.digestLength as usize;
        }
        res = TEE_ERROR_SHORT_BUFFER;
    }

    if res == TEE_ERROR_SHORT_BUFFER {
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 设置默认错误状态
    //res = TEE_ERROR_GENERIC;

    // 准备参数
    let mut acc_dlen = 0usize;
    let mut tl = tag_len_val as u64;
    let mut tmp_dlen = (dest_len_val - acc_dlen) as u64;

    let dst = dest_data as *mut u8;

    // 根据块大小选择处理路径
    if op_handle.block_size > 1 {
        // 使用缓冲区更新函数处理块数据
        res = tee_buffer_update(
            unsafe { &mut *operation },
            _utee_authenc_update_payload,
            src_data,
            src_len,
            dest_data,
            &mut tmp_dlen,
        );
        if res != TEE_SUCCESS {
            if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
                TEE_Panic(res as u32);
            }
            return res;
        }

        acc_dlen += tmp_dlen as usize;
        tmp_dlen = (dest_len_val - acc_dlen) as u64;

        // 调用最终加密函数
        let buffer_ptr = op_handle.buffer as *const core::ffi::c_void;
        res = unsafe {
            _utee_authenc_enc_final(
                op_handle.state as u64,
                buffer_ptr,
                op_handle.buffer_offs,
                dst.add(acc_dlen) as *mut core::ffi::c_void,
                &mut tmp_dlen,
                tag,
                &mut tl,
            ) as TEE_Result
        };
    } else {
        // 直接处理非块数据
        res = unsafe {
            _utee_authenc_enc_final(
                op_handle.state as u64,
                src_data,
                src_len,
                dst as *mut core::ffi::c_void,
                &mut tmp_dlen,
                tag,
                &mut tl,
            ) as TEE_Result
        };
    }

    // 更新标签长度
    unsafe {
        *tag_len = tl as usize;
    }

    if res != TEE_SUCCESS {
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
            TEE_Panic(res as u32);
        }
        return res;
    }

    acc_dlen += tmp_dlen as usize;
    unsafe {
        *dest_len = acc_dlen;
    }

    // 清除已初始化标志
    unsafe {
        (*operation).info.handleState &= !TEE_HANDLE_FLAG_INITIALIZED;
        (*operation).operation_state = TEE_OPERATION_STATE_INITIAL;
    }

    // 检查返回结果
    if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
        TEE_Panic(res as u32);
    }

    res
}

/// AE (Authenticated Encryption) 解密最终操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `src_data` - 源数据指针
/// * `src_len` - 源数据长度
/// * `dest_data` - 目标缓冲区指针
/// * `dest_len` - 指向目标缓冲区大小的指针，返回实际写入的数据长度
/// * `tag` - 认证标签缓冲区指针
/// * `tag_len` - 标签缓冲区长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_SHORT_BUFFER` - 缓冲区太小
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_MAC_INVALID` - MAC验证失败
#[unsafe(no_mangle)]
pub extern "C" fn TEE_AEDecryptFinal(
    operation: *mut TEE_OperationHandle,
    src_data: *const core::ffi::c_void,
    src_len: usize,
    dest_data: *mut core::ffi::c_void,
    dest_len: *mut usize,
    tag: *const core::ffi::c_void,
    tag_len: usize,
) -> TEE_Result {
    let mut res = TEE_SUCCESS;
    // 检查操作句柄和源数据参数
    if operation.is_null() || (src_data.is_null() && src_len > 0) {
        let res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER && res != TEE_ERROR_MAC_INVALID {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 检查dest_len参数的内存访问权限
    if !dest_len.is_null() {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
                dest_len as *mut core::ffi::c_void,
                std::mem::size_of::<usize>(),
            );
            if check_res != 0 {
                eprintln!("[inout] destLen: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    } else {
        let res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER && res != TEE_ERROR_MAC_INVALID {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 获取操作句柄
    let op_handle = unsafe { &*operation };

    // 检查操作类型
    if op_handle.info.operationClass != TEE_OPERATION_AE {
        let res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER && res != TEE_ERROR_MAC_INVALID {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 检查操作是否已初始化
    if (op_handle.info.handleState & TEE_HANDLE_FLAG_INITIALIZED) == 0 {
        let res = TEE_ERROR_BAD_PARAMETERS;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER && res != TEE_ERROR_MAC_INVALID {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 计算所需的缓冲区大小
    let req_dlen = op_handle.buffer_offs + src_len;

    // 检查目标缓冲区是否足够大
    let dest_len_val = unsafe { *dest_len };
    if dest_len_val < req_dlen {
        unsafe {
            *dest_len = req_dlen;
        }
        let res = TEE_ERROR_SHORT_BUFFER;
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER && res != TEE_ERROR_MAC_INVALID {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 准备参数
    let mut acc_dlen = 0usize;
    let mut tmp_dlen = (dest_len_val - acc_dlen) as u64;

    let dst = dest_data as *mut u8;

    // 根据块大小选择处理路径
    if op_handle.block_size > 1 {
        // 使用缓冲区更新函数处理块数据
        res = tee_buffer_update(
            unsafe { &mut *operation },
            _utee_authenc_update_payload,
            src_data,
            src_len,
            dest_data,
            &mut tmp_dlen,
        );
        if res != TEE_SUCCESS {
            if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER && res != TEE_ERROR_MAC_INVALID {
                TEE_Panic(res as u32);
            }
            return res;
        }

        acc_dlen += tmp_dlen as usize;
        tmp_dlen = (dest_len_val - acc_dlen) as u64;

        // 调用最终解密函数
        let buffer_ptr = op_handle.buffer as *const core::ffi::c_void;
        res = unsafe {
            _utee_authenc_dec_final(
                op_handle.state as u64,
                buffer_ptr,
                op_handle.buffer_offs,
                dst.add(acc_dlen) as *mut core::ffi::c_void,
                &mut tmp_dlen,
                tag,
                tag_len,
            ) as TEE_Result
        };
    } else {
        // 直接处理非块数据
        res = unsafe {
            _utee_authenc_dec_final(
                op_handle.state as u64,
                src_data,
                src_len,
                dst as *mut core::ffi::c_void,
                &mut tmp_dlen,
                tag,
                tag_len,
            ) as TEE_Result
        };
    }

    if res != TEE_SUCCESS {
        if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER && res != TEE_ERROR_MAC_INVALID {
            TEE_Panic(res as u32);
        }
        return res;
    }

    // 检查提供的标签长度是否与初始化时的长度匹配
    if tag_len != op_handle.info.digestLength as usize {
        res = TEE_ERROR_MAC_INVALID;
    }

    acc_dlen += tmp_dlen as usize;
    unsafe {
        *dest_len = acc_dlen;
    }

    // 清除已初始化标志
    unsafe {
        (*operation).info.handleState &= !TEE_HANDLE_FLAG_INITIALIZED;
        (*operation).operation_state = TEE_OPERATION_STATE_INITIAL;
    }

    // 检查返回结果
    if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER && res != TEE_ERROR_MAC_INVALID {
        TEE_Panic(res as u32);
    }

    res
}

/// 非对称加密操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `params` - 参数属性数组指针
/// * `param_count` - 参数数量
/// * `src_data` - 源数据指针
/// * `src_len` - 源数据长度
/// * `dest_data` - 目标缓冲区指针
/// * `dest_len` - 指向目标缓冲区大小的指针，返回实际写入的数据长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_SHORT_BUFFER` - 缓冲区太小
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_CIPHERTEXT_INVALID` - 密文无效
/// * `TEE_ERROR_NOT_SUPPORTED` - 不支持的操作
#[unsafe(no_mangle)]
pub extern "C" fn TEE_AsymmetricEncrypt(
    operation: *mut TEE_OperationHandle,
    params: *const TEE_Attribute,
    param_count: u32,
    src_data: *const core::ffi::c_void,
    src_len: usize,
    dest_data: *mut core::ffi::c_void,
    dest_len: *mut usize,
) -> TEE_Result {
    let mut res = TEE_SUCCESS;

    // 检查操作句柄和源数据参数
    if operation.is_null() || (src_data.is_null() && src_len > 0) {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查参数数组的内存访问权限
    if !params.is_null() && param_count > 0 {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ,
                params as *mut core::ffi::c_void,
                (param_count as usize) * std::mem::size_of::<TEE_Attribute>(),
            );
            if check_res != 0 {
                eprintln!("[in] params: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    }

    // 检查目标长度参数的内存访问权限
    if !dest_len.is_null() {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
                dest_len as *mut core::ffi::c_void,
                std::mem::size_of::<usize>(),
            );
            if check_res != 0 {
                eprintln!("[inout] destLen: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    } else {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取操作句柄
    let op_handle = unsafe { &*operation };

    // 检查操作句柄中的密钥
    if op_handle.key1.is_null() {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查操作类型和模式
    if op_handle.info.operationClass != TEE_OPERATION_ASYMMETRIC_CIPHER {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    if op_handle.info.mode != TEE_MODE_ENCRYPT {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 从系统调用获取实际的缓冲区大小
    let mut dl = unsafe { *dest_len as u64 };

    // 分配并转换属性数组
    let mut ua = Vec::with_capacity(param_count as usize);
    ua.resize(
        param_count as usize,
        crate::utee_types::utee_attribute::default(),
    );
    unsafe {
        __utee_from_attr(ua.as_mut_ptr(), params, param_count);
    }

    // 调用底层非对称加密操作
    let syscall_res = unsafe {
        _utee_asymm_operate(
            op_handle.state as u64,
            ua.as_ptr(),
            param_count as u64,
            src_data,
            src_len,
            dest_data,
            &mut dl,
        )
    };

    // 更新实际写入长度
    unsafe {
        *dest_len = dl as usize;
    }

    res = syscall_res as TEE_Result;

    // 检查返回结果
    if res != TEE_SUCCESS
        && res != TEE_ERROR_SHORT_BUFFER
        && res != TEE_ERROR_BAD_PARAMETERS
        && res != TEE_ERROR_CIPHERTEXT_INVALID
        && res != TEE_ERROR_NOT_SUPPORTED
    {
        TEE_Panic(res as u32);
    }

    res
}

/// 非对称解密操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `params` - 参数属性数组指针
/// * `param_count` - 参数数量
/// * `src_data` - 源数据指针
/// * `src_len` - 源数据长度
/// * `dest_data` - 目标缓冲区指针
/// * `dest_len` - 指向目标缓冲区大小的指针，返回实际写入的数据长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_SHORT_BUFFER` - 缓冲区太小
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
/// * `TEE_ERROR_CIPHERTEXT_INVALID` - 密文无效
/// * `TEE_ERROR_NOT_SUPPORTED` - 不支持的操作
#[unsafe(no_mangle)]
pub extern "C" fn TEE_AsymmetricDecrypt(
    operation: *mut TEE_OperationHandle,
    params: *const TEE_Attribute,
    param_count: u32,
    src_data: *const core::ffi::c_void,
    src_len: usize,
    dest_data: *mut core::ffi::c_void,
    dest_len: *mut usize,
) -> TEE_Result {
    let mut res = TEE_SUCCESS;

    // 检查操作句柄和源数据参数
    if operation.is_null() || (src_data.is_null() && src_len > 0) {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查参数数组的内存访问权限
    if !params.is_null() && param_count > 0 {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ,
                params as *mut core::ffi::c_void,
                (param_count as usize) * std::mem::size_of::<TEE_Attribute>(),
            );
            if check_res != 0 {
                eprintln!("[in] params: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    }

    // 检查目标长度参数的内存访问权限
    if !dest_len.is_null() {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
                dest_len as *mut core::ffi::c_void,
                std::mem::size_of::<usize>(),
            );
            if check_res != 0 {
                eprintln!("[inout] destLen: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    } else {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取操作句柄
    let op_handle = unsafe { &*operation };

    // 检查操作句柄中的密钥
    if op_handle.key1.is_null() {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查操作类型和模式
    if op_handle.info.operationClass != TEE_OPERATION_ASYMMETRIC_CIPHER {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    if op_handle.info.mode != TEE_MODE_DECRYPT {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 从系统调用获取实际的缓冲区大小
    let mut dl = unsafe { *dest_len as u64 };

    // 分配并转换属性数组
    let mut ua = Vec::with_capacity(param_count as usize);
    ua.resize(
        param_count as usize,
        crate::utee_types::utee_attribute::default(),
    );
    unsafe {
        __utee_from_attr(ua.as_mut_ptr(), params, param_count);
    }

    // 调用底层非对称解密操作
    let syscall_res = unsafe {
        _utee_asymm_operate(
            op_handle.state as u64,
            ua.as_ptr(),
            param_count as u64,
            src_data,
            src_len,
            dest_data,
            &mut dl,
        )
    };

    // 更新实际写入长度
    unsafe {
        *dest_len = dl as usize;
    }

    // 检查返回结果
    res = syscall_res as TEE_Result;
    if res != TEE_SUCCESS
        && res != TEE_ERROR_SHORT_BUFFER
        && res != TEE_ERROR_BAD_PARAMETERS
        && res != TEE_ERROR_CIPHERTEXT_INVALID
        && res != TEE_ERROR_NOT_SUPPORTED
    {
        TEE_Panic(res as u32);
    }

    res
}

/// 非对称签名摘要操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `params` - 参数属性数组指针
/// * `param_count` - 参数数量
/// * `digest` - 摘要数据指针
/// * `digest_len` - 摘要数据长度
/// * `signature` - 签名缓冲区指针
/// * `signature_len` - 指向签名缓冲区大小的指针，返回实际签名长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_SHORT_BUFFER` - 缓冲区太小
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
#[unsafe(no_mangle)]
pub extern "C" fn TEE_AsymmetricSignDigest(
    operation: *mut TEE_OperationHandle,
    params: *const TEE_Attribute,
    param_count: u32,
    digest: *const core::ffi::c_void,
    digest_len: usize,
    signature: *mut core::ffi::c_void,
    signature_len: *mut usize,
) -> TEE_Result {
    let mut res = TEE_SUCCESS;

    // 检查操作句柄和摘要参数
    if operation.is_null() || (digest.is_null() && digest_len > 0) {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查参数数组的内存访问权限
    if !params.is_null() && param_count > 0 {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ,
                params as *mut core::ffi::c_void,
                (param_count as usize) * std::mem::size_of::<TEE_Attribute>(),
            );
            if check_res != 0 {
                eprintln!("[in] params: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    }

    // 检查签名长度参数的内存访问权限
    if !signature_len.is_null() {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE,
                signature_len as *mut core::ffi::c_void,
                std::mem::size_of::<usize>(),
            );
            if check_res != 0 {
                eprintln!("[inout] signatureLen: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    } else {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 获取操作句柄
    let op_handle = unsafe { &*operation };

    // 检查操作句柄中的密钥
    if op_handle.key1.is_null() {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查操作类型和模式
    if op_handle.info.operationClass != TEE_OPERATION_ASYMMETRIC_SIGNATURE {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    if op_handle.info.mode != TEE_MODE_SIGN {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 从系统调用获取实际的缓冲区大小
    let mut sl = unsafe { *signature_len as u64 };

    // 分配并转换属性数组
    let mut ua = Vec::with_capacity(param_count as usize);
    ua.resize(
        param_count as usize,
        crate::utee_types::utee_attribute::default(),
    );
    unsafe {
        __utee_from_attr(ua.as_mut_ptr(), params, param_count);
    }

    // 调用底层非对称签名操作
    let syscall_res = unsafe {
        _utee_asymm_operate(
            op_handle.state as u64,
            ua.as_ptr(),
            param_count as u64,
            digest,
            digest_len,
            signature,
            &mut sl,
        )
    };

    // 更新实际签名长度
    unsafe {
        *signature_len = sl as usize;
    }

    // 检查返回结果
    res = syscall_res as TEE_Result;
    if res != TEE_SUCCESS && res != TEE_ERROR_SHORT_BUFFER {
        TEE_Panic(res as u32);
    }

    res
}

/// 非对称验证摘要操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `params` - 参数属性数组指针
/// * `param_count` - 参数数量
/// * `digest` - 摘要数据指针
/// * `digest_len` - 摘要数据长度
/// * `signature` - 签名数据指针
/// * `signature_len` - 签名数据长度
///
/// # 返回值
/// * `TEE_SUCCESS` - 成功
/// * `TEE_ERROR_SIGNATURE_INVALID` - 签名无效
/// * `TEE_ERROR_BAD_PARAMETERS` - 参数错误
#[unsafe(no_mangle)]
pub extern "C" fn TEE_AsymmetricVerifyDigest(
    operation: *mut TEE_OperationHandle,
    params: *const TEE_Attribute,
    param_count: u32,
    digest: *const core::ffi::c_void,
    digest_len: usize,
    signature: *const core::ffi::c_void,
    signature_len: usize,
) -> TEE_Result {
    let mut res = TEE_SUCCESS;

    // 检查操作句柄和参数的一致性
    if operation.is_null()
        || (digest.is_null() && digest_len != 0)
        || (signature.is_null() && signature_len != 0)
    {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查参数数组的内存访问权限
    if !params.is_null() && param_count > 0 {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ,
                params as *mut core::ffi::c_void,
                (param_count as usize) * std::mem::size_of::<TEE_Attribute>(),
            );
            if check_res != 0 {
                eprintln!("[in] params: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    }

    // 检查摘要数据的内存访问权限
    if !digest.is_null() && digest_len > 0 {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ,
                digest as *mut core::ffi::c_void,
                digest_len,
            );
            if check_res != 0 {
                eprintln!("[in] digest: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    }

    // 检查签名数据的内存访问权限
    if !signature.is_null() && signature_len > 0 {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ,
                signature as *mut core::ffi::c_void,
                signature_len,
            );
            if check_res != 0 {
                eprintln!("[in] signature: error {:#010x}", check_res);
                TEE_Panic(0);
                return TEE_ERROR_BAD_PARAMETERS;
            }
        }
    }

    // 获取操作句柄
    let op_handle = unsafe { &*operation };

    // 检查操作句柄中的密钥
    if op_handle.key1.is_null() {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 检查操作类型和模式
    if op_handle.info.operationClass != TEE_OPERATION_ASYMMETRIC_SIGNATURE {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    if op_handle.info.mode != TEE_MODE_VERIFY {
        TEE_Panic(0);
        return TEE_ERROR_BAD_PARAMETERS;
    }

    // 分配并转换属性数组
    let mut ua = Vec::with_capacity(param_count as usize);
    ua.resize(
        param_count as usize,
        crate::utee_types::utee_attribute::default(),
    );
    unsafe {
        __utee_from_attr(ua.as_mut_ptr(), params, param_count);
    }

    // 调用底层非对称验证操作
    let syscall_res = unsafe {
        _utee_asymm_verify(
            op_handle.state as u64,
            ua.as_ptr(),
            param_count as u64,
            digest,
            digest_len,
            signature,
            signature_len,
        )
    };

    // 检查返回结果
    res = syscall_res as TEE_Result;
    if res != TEE_SUCCESS && res != TEE_ERROR_SIGNATURE_INVALID {
        TEE_Panic(res as u32);
    }

    res
}

/// 派生密钥操作
///
/// # 参数
/// * `operation` - 操作句柄
/// * `params` - 参数属性数组指针
/// * `param_count` - 参数数量
/// * `derived_key` - 派生密钥对象句柄
#[unsafe(no_mangle)]
pub extern "C" fn TEE_DeriveKey(
    operation: *mut TEE_OperationHandle,
    params: *const TEE_Attribute,
    param_count: u32,
    derived_key: TEE_ObjectHandle,
) {
    // 检查操作句柄和派生密钥参数
    if operation.is_null() || derived_key.is_null() {
        TEE_Panic(0);
        return;
    }

    // 检查参数数组的内存访问权限
    if !params.is_null() && param_count > 0 {
        if cfg!(feature = "strict_annotation_checks") {
            let check_res = TEE_CheckMemoryAccessRights(
                TEE_MEMORY_ACCESS_READ,
                params as *mut core::ffi::c_void,
                (param_count as usize) * std::mem::size_of::<TEE_Attribute>(),
            );
            if check_res != 0 {
                eprintln!("[in] params: error {:#010x}", check_res);
                TEE_Panic(0);
                return;
            }
        }
    }

    // 获取操作句柄
    let op_handle = unsafe { &*operation };

    // 检查操作算法类别
    if TEE_ALG_GET_CLASS(op_handle.info.algorithm) != TEE_OPERATION_KEY_DERIVATION {
        TEE_Panic(0);
        return;
    }

    // 检查操作类型和模式
    if op_handle.info.operationClass != TEE_OPERATION_KEY_DERIVATION {
        TEE_Panic(0);
        return;
    }

    if op_handle.key1.is_null() {
        TEE_Panic(0);
        return;
    }

    if op_handle.info.mode != TEE_MODE_DERIVE {
        TEE_Panic(0);
        return;
    }

    if (op_handle.info.handleState & TEE_HANDLE_FLAG_KEY_SET) == 0 {
        TEE_Panic(0);
        return;
    }

    // 获取派生密钥对象信息
    let mut key_info = crate::utee_types::utee_object_info::default();

    let res = unsafe { _utee_cryp_obj_get_info(derived_key as u64, &mut key_info) };
    if res != TEE_SUCCESS as usize {
        TEE_Panic(res as u32);
        return;
    }

    // 检查派生密钥对象类型和状态
    if key_info.obj_type != TEE_TYPE_GENERIC_SECRET {
        TEE_Panic(0);
        return;
    }

    if (key_info.handle_flags & TEE_HANDLE_FLAG_INITIALIZED) != 0 {
        TEE_Panic(0);
        return;
    }

    // 分配并转换属性数组
    let mut ua = Vec::with_capacity(param_count as usize);
    ua.resize(
        param_count as usize,
        crate::utee_types::utee_attribute::default(),
    );
    unsafe {
        __utee_from_attr(ua.as_mut_ptr(), params, param_count);
    }

    // 调用底层密钥派生操作
    let res = unsafe {
        _utee_cryp_derive_key(
            op_handle.state as u64,
            ua.as_ptr(),
            param_count as u64,
            derived_key as u64,
        )
    };

    if res != TEE_SUCCESS as usize {
        TEE_Panic(res as u32);
    }
}

/// 生成随机数
///
/// # 参数
/// * `random_buffer` - 接收随机数的缓冲区指针
/// * `random_buffer_len` - 缓冲区长度
#[unsafe(no_mangle)]
pub extern "C" fn TEE_GenerateRandom(
    random_buffer: *mut core::ffi::c_void,
    random_buffer_len: usize,
) {
    // 检查参数的有效性
    if random_buffer.is_null() && random_buffer_len > 0 {
        TEE_Panic(TEE_ERROR_BAD_PARAMETERS as u32);
        return;
    }

    // 调用底层随机数生成函数
    let res = unsafe { _utee_cryp_random_number_generate(random_buffer, random_buffer_len) };

    if res != TEE_SUCCESS as usize {
        TEE_Panic(res as u32);
    }
}

/// 检查算法是否受支持
///
/// # 参数
/// * `alg` - 算法标识符
/// * `element` - 算法元素（如椭圆曲线）
///
/// # 返回值
/// * `TEE_SUCCESS` - 算法受支持
/// * `TEE_ERROR_NOT_SUPPORTED` - 算法不受支持
#[unsafe(no_mangle)]
pub extern "C" fn TEE_IsAlgorithmSupported(alg: u32, element: u32) -> TEE_Result {
    use crate::tee_api_defines::*;

    // 检查AES相关算法
    if alg == TEE_ALG_AES_ECB_NOPAD {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_AES_CBC_NOPAD {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_AES_CTR {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_AES_CTS {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_AES_XTS {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_AES_CBC_MAC_NOPAD || alg == TEE_ALG_AES_CBC_MAC_PKCS5 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_AES_CMAC {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_AES_CCM {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_AES_GCM {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查DES相关算法
    if alg == TEE_ALG_DES_ECB_NOPAD || alg == TEE_ALG_DES3_ECB_NOPAD {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_DES_CBC_NOPAD || alg == TEE_ALG_DES3_CBC_NOPAD {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_DES_CBC_MAC_NOPAD
        || alg == TEE_ALG_DES_CBC_MAC_PKCS5
        || alg == TEE_ALG_DES3_CBC_MAC_NOPAD
        || alg == TEE_ALG_DES3_CBC_MAC_PKCS5
    {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查MD5
    if alg == TEE_ALG_MD5 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查SHA1
    if alg == TEE_ALG_SHA1 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查SHA224
    if alg == TEE_ALG_SHA224 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查SHA256
    if alg == TEE_ALG_SHA256 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查SHA384
    if alg == TEE_ALG_SHA384 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查SHA512
    if alg == TEE_ALG_SHA512 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查MD5SHA1
    if alg == TEE_ALG_MD5SHA1 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查HMAC相关算法
    if alg == TEE_ALG_HMAC_MD5 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_HMAC_SHA1 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_HMAC_SHA224 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_HMAC_SHA256 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_HMAC_SHA384 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_HMAC_SHA512 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_HMAC_SM3 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查SM3
    if alg == TEE_ALG_SM3 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查SM4相关算法
    if alg == TEE_ALG_SM4_ECB_NOPAD {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_SM4_CBC_NOPAD {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_SM4_CTR {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查RSA相关算法
    if alg == TEE_ALG_RSASSA_PKCS1_V1_5_MD5 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_RSASSA_PKCS1_V1_5_SHA1
        || alg == TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA1
        || alg == TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA1
    {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_RSASSA_PKCS1_V1_5_MD5SHA1 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_RSASSA_PKCS1_V1_5_SHA224
        || alg == TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA224
        || alg == TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA224
    {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_RSASSA_PKCS1_V1_5_SHA256
        || alg == TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA256
        || alg == TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA256
    {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_RSASSA_PKCS1_V1_5_SHA384
        || alg == TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA384
        || alg == TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA384
    {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_RSASSA_PKCS1_V1_5_SHA512
        || alg == TEE_ALG_RSASSA_PKCS1_PSS_MGF1_SHA512
        || alg == TEE_ALG_RSAES_PKCS1_OAEP_MGF1_SHA512
    {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_RSA_NOPAD {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查DSA相关算法
    if alg == TEE_ALG_DSA_SHA1 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_DSA_SHA224 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    if alg == TEE_ALG_DSA_SHA256 {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查DH
    if alg == TEE_ALG_DH_DERIVE_SHARED_SECRET {
        if element == TEE_CRYPTO_ELEMENT_NONE {
            return TEE_SUCCESS;
        }
    }

    // 检查ECC相关算法
    if (alg == TEE_ALG_ECDH_P192
        || alg == TEE_ALG_ECDSA_P192
        || alg == TEE_ALG_DH_DERIVE_SHARED_SECRET)
        && element == TEE_ECC_CURVE_NIST_P192
    {
        return TEE_SUCCESS;
    }

    if (alg == TEE_ALG_ECDH_P224
        || alg == TEE_ALG_ECDSA_P224
        || alg == TEE_ALG_DH_DERIVE_SHARED_SECRET)
        && element == TEE_ECC_CURVE_NIST_P224
    {
        return TEE_SUCCESS;
    }

    if (alg == TEE_ALG_ECDH_P256
        || alg == TEE_ALG_ECDSA_P256
        || alg == TEE_ALG_DH_DERIVE_SHARED_SECRET)
        && element == TEE_ECC_CURVE_NIST_P256
    {
        return TEE_SUCCESS;
    }

    if (alg == TEE_ALG_ECDH_P384
        || alg == TEE_ALG_ECDSA_P384
        || alg == TEE_ALG_DH_DERIVE_SHARED_SECRET)
        && element == TEE_ECC_CURVE_NIST_P384
    {
        return TEE_SUCCESS;
    }

    if (alg == TEE_ALG_ECDH_P521
        || alg == TEE_ALG_ECDSA_P521
        || alg == TEE_ALG_DH_DERIVE_SHARED_SECRET)
        && element == TEE_ECC_CURVE_NIST_P521
    {
        return TEE_SUCCESS;
    }

    // 检查SM2_DSA
    if alg == TEE_ALG_SM2_DSA_SM3 && element == TEE_ECC_CURVE_SM2 {
        return TEE_SUCCESS;
    }

    // 检查SM2_KEP
    if alg == TEE_ALG_SM2_KEP && element == TEE_ECC_CURVE_SM2 {
        return TEE_SUCCESS;
    }

    // 检查SM2_PKE
    if alg == TEE_ALG_SM2_PKE && element == TEE_ECC_CURVE_SM2 {
        return TEE_SUCCESS;
    }

    TEE_ERROR_NOT_SUPPORTED
}
