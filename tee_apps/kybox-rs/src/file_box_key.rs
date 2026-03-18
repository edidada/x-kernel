// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::ptr;

use crate::get_box_cipher_info;
use rand::{thread_rng, Rng};
use xtee_utee::{
    api::tee_api_objects::{
        TEE_CloseAndDeletePersistentObject, TEE_CloseObject, TEE_CreatePersistentObject,
        TEE_OpenPersistentObject, TEE_ReadObjectData, TEE_WriteObjectData,
    },
    error::{Error, Result},
    tee_api_defines::{
        TEE_DATA_FLAG_ACCESS_READ, TEE_DATA_FLAG_ACCESS_WRITE, TEE_DATA_FLAG_ACCESS_WRITE_META,
        TEE_DATA_FLAG_OVERWRITE, TEE_STORAGE_PRIVATE, TEE_SUCCESS,
    },
    tee_api_types::TEE_ObjectHandle,
    trace_println,
};

const KEY_AUTH_SIZE: usize = 32;

/// 生成保护箱密钥、初始向量和认证密钥
///
/// 使用密码学安全随机数生成器生成三个组件：
/// 1. 密钥认证数据（key_auth）- 用于密钥验证
/// 2. 加密密钥（key）- 用于数据加解密
/// 3. 初始向量（iv）- 用于加密算法
///
/// # 返回
/// - `Ok((key_auth, key, iv))`: 成功生成的密钥三元组
/// - `Err(Error::Generic)`: 获取密码算法信息失败
pub fn cc_generate_box_key() -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let (key_size, iv_size, _) = get_box_cipher_info().map_err(|e| {
        trace_println!("TA KyBox: failed to get box cipher info: {}", e);
        Error::Generic
    })?;
    let mut key = vec![0u8; key_size];
    let mut iv = vec![0u8; iv_size];
    let mut key_auth = vec![0u8; KEY_AUTH_SIZE];
    let mut rng = thread_rng();

    rng.fill_bytes(&mut key);
    rng.fill_bytes(&mut iv);
    rng.fill_bytes(&mut key_auth);

    Ok((key_auth, key, iv))
}

/// 保存保护箱密钥到安全存储
///
/// # 参数
/// - `key`: 加密密钥（包含认证数据）
/// - `iv`: 初始向量
/// - `key_id`: 密钥标识符
///
/// # 返回
/// - `Ok(())`: 保存成功
/// - `Err(Error::from_raw_error(res))`: TEE 错误
pub fn cc_save_box_key(key: &[u8], iv: &[u8], key_id: &str) -> Result<()> {
    let mut object: TEE_ObjectHandle = ptr::null_mut();
    let obj_data_flag = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;
    let obj_id = key_id;

    let res = TEE_CreatePersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len(),
        obj_data_flag,
        ptr::null_mut(),
        ptr::null(),
        0,
        &mut object,
    );

    if res != TEE_SUCCESS {
        trace_println!("TA KyBox: failed to open object");
        return Err(Error::from(res));
    }

    let mut key_data = vec![0u8; key.len() + iv.len()];

    key_data[..key.len()].copy_from_slice(key);
    key_data[key.len()..].copy_from_slice(iv);

    let res = TEE_WriteObjectData(object, key_data.as_ptr() as *const _, key_data.len());
    if res != TEE_SUCCESS {
        trace_println!("TA KyBox: failed to write object data");
        return Err(Error::from(res));
    }

    TEE_CloseObject(object);

    Ok(())
}

/// 从安全存储获取保护箱密钥
///
/// # 参数
/// - `key`: 输出缓冲区，用于接收加密密钥
/// - `iv`: 输出缓冲区，用于接收初始向量
/// - `key_id`: 密钥标识符
///
/// # 返回
/// - `Ok(())`: 获取成功，密钥已填充到缓冲区
/// - `Err(Error::Overflow)`: 缓冲区大小不正确
/// - `Err(Error::from_raw_error(res))`: TEE 错误
pub fn cc_get_box_key(key: &mut [u8], iv: &mut [u8], key_id: &str) -> Result<()> {
    let (key_size, iv_size, _) = get_box_cipher_info().map_err(|e| {
        trace_println!("TA KyBox: failed to get box cipher info: {}", e);
        Error::Generic
    })?;

    if key.len() != key_size || iv.len() != iv_size {
        trace_println!("TA KyBox: cc_get_box_key: 缓冲区大小错误");
        return Err(Error::Overflow);
    }

    let mut object: TEE_ObjectHandle = ptr::null_mut();
    let obj_read_flag = TEE_DATA_FLAG_ACCESS_READ;
    let obj_id = key_id;
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len(),
        obj_read_flag,
        &mut object,
    );

    if res != TEE_SUCCESS {
        trace_println!("TA KyBox: failed to open object");
        return Err(Error::from(res));
    }

    let box_key_len = KEY_AUTH_SIZE + key_size;
    let mut key_data = vec![0u8; box_key_len + iv_size];
    let mut read_data_len = 0;

    let res = TEE_ReadObjectData(
        object,
        key_data.as_mut_ptr() as *mut _,
        key_data.len(),
        &mut read_data_len,
    );

    if res != TEE_SUCCESS {
        trace_println!("TA KyBox: failed to open object");
        return Err(Error::from(res));
    }

    key.copy_from_slice(&key_data[KEY_AUTH_SIZE..box_key_len]);
    iv.copy_from_slice(&key_data[box_key_len..]);

    TEE_CloseObject(object);

    Ok(())
}

/// 获取密钥的认证数据部分
///
/// # 参数
/// - `key`: 输出缓冲区，用于接收密钥认证数据
/// - `key_id`: 密钥标识符
///
/// # 返回
/// - `Ok(())`: 获取成功
/// - `Err(Error::Overflow)`: 缓冲区大小不正确
/// - `Err(Error::from_raw_error(res))`: TEE 错误
pub fn cc_get_box_key_auth(key: &mut [u8], key_id: &str) -> Result<()> {
    let (key_size, iv_size, _) = get_box_cipher_info().map_err(|e| {
        trace_println!("TA KyBox: failed to get box cipher info: {}", e);
        Error::Generic
    })?;

    if key.len() != KEY_AUTH_SIZE {
        trace_println!("TA KyBox:: cc_get_box_key_auth: 缓冲区大小错误");
        return Err(Error::Overflow);
    }

    let mut object: TEE_ObjectHandle = ptr::null_mut();
    let obj_read_flag = TEE_DATA_FLAG_ACCESS_READ;
    let obj_id = key_id;
    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len(),
        obj_read_flag,
        &mut object,
    );

    if res != TEE_SUCCESS {
        trace_println!("TA KyBox: failed to open object");
        return Err(Error::from(res));
    }

    let box_key_len = KEY_AUTH_SIZE + key_size;
    let mut key_data = vec![0u8; box_key_len + iv_size];
    let mut read_data_len = 0;

    let res = TEE_ReadObjectData(
        object,
        key_data.as_mut_ptr() as *mut _,
        key_data.len(),
        &mut read_data_len,
    );

    if res != TEE_SUCCESS {
        trace_println!("TA KyBox: failed to open object");
        return Err(Error::from(res));
    }

    key.copy_from_slice(&key_data[..KEY_AUTH_SIZE]);

    TEE_CloseObject(object);

    Ok(())
}

/// 从安全存储删除保护箱密钥
///
/// # 参数
/// - `key_id`: 要删除的密钥标识符
///
/// # 返回
/// - `Ok(())`: 删除成功
/// - ``Err(Error::from_raw_error(res))`: TEE 错误
pub fn cc_destroy_box_key(key_id: &str) -> Result<()> {
    let mut object: TEE_ObjectHandle = ptr::null_mut();
    let obj_delete_flag = TEE_DATA_FLAG_ACCESS_WRITE_META;
    let obj_id = key_id;

    let res = TEE_OpenPersistentObject(
        TEE_STORAGE_PRIVATE,
        obj_id.as_ptr() as *const _,
        obj_id.len(),
        obj_delete_flag,
        &mut object,
    );

    if res != TEE_SUCCESS {
        trace_println!("TA KyBox: failed to open object");
        return Err(Error::from(res));
    }

    TEE_CloseAndDeletePersistentObject(object);

    Ok(())
}

/// 检查指定密钥是否存在
///
/// # 参数
/// - `key_id`: 要检查的密钥标识符
///
/// # 返回
/// - `Ok(true)`: 密钥存在
/// - `Ok(false)`: 密钥不存在
/// - `Err`: 检查过程中发生错误
pub fn cc_box_key_is_exist(key_id: &str) -> Result<bool> {
    let (key_size, iv_size, _) = get_box_cipher_info().map_err(|e| {
        trace_println!("TA KyBox: failed to get box cipher info: {}", e);
        Error::Generic
    })?;
    let mut key = vec![0u8; key_size];
    let mut iv = vec![0u8; iv_size];

    match cc_get_box_key(&mut key, &mut iv, key_id) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// 验证提供的密钥认证数据是否匹配存储的密钥
///
/// # 参数
/// - `key`: 待验证的密钥认证数据
/// - `key_id`: 密钥标识符
///
/// # 返回
/// - `Ok(true)`: 验证通过
/// - `Ok(false)`: 验证失败
/// - `Err(Error::BadParameters)`: 输入参数大小不正确
/// - `Err`: 获取密钥失败
pub fn cc_verify_box_key(key: &[u8], key_id: &str) -> Result<bool> {
    let mut stored_key = vec![0u8; KEY_AUTH_SIZE];

    if key.len() != stored_key.len() {
        return Err(Error::BadParameters);
    }

    match cc_get_box_key_auth(&mut stored_key, key_id) {
        Ok(_) => {
            if stored_key == key {
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Err(e) => Err(e),
    }
}
