#![allow(unused_imports)]

use std::{ptr, string::String, vec, vec::Vec};

mod file_box_algo;
mod proto;

use file_box_algo::{decrypt_box_data, encrypt_box_data, get_box_cipher_info};
use rand::{Rng, thread_rng};
use xtee_utee::{
    api::tee_api_objects::{
        TEE_CloseAndDeletePersistentObject, TEE_CloseObject, TEE_CreatePersistentObject,
        TEE_OpenPersistentObject, TEE_ReadObjectData, TEE_WriteObjectData,
    },
    error::{Error, Result},
    ta_manager::{TAManager, TrustedApplication},
    tee_api_defines::{
        TEE_DATA_FLAG_ACCESS_READ, TEE_DATA_FLAG_ACCESS_WRITE, TEE_DATA_FLAG_ACCESS_WRITE_META,
        TEE_DATA_FLAG_OVERWRITE, TEE_PARAM_TYPES, TEE_STORAGE_PRIVATE, TEE_SUCCESS,
    },
    tee_api_types::TEE_ObjectHandle,
    trace_println,
    wire::{ParamType, Parameters},
};

use crate::proto::{Command, UUID};

const KEY_AUTH_SIZE: usize = 32;

/// 验证参数类型是否匹配预期
///
/// # 参数
/// - `cmd_id`: 当前执行的命令 ID，用于错误信息
/// - `pt`: 实际参数类型
/// - `exp_pt`: 期望的参数类型
///
/// # 返回
/// - `Ok(())`: 参数类型匹配
/// - `Err(Error::BadParameters)`: 参数类型不匹配
pub fn expect_param_types(cmd_id: u32, pt: u32, exp_pt: u32) -> Result<()> {
    if pt != exp_pt {
        trace_println!(
            "TA KyBox: cmd '{}' param types 不匹配，期望 0x{:08x}，实际 0x{:08x}",
            cmd_id,
            exp_pt,
            pt
        );
        return Err(Error::BadParameters);
    }
    Ok(())
}

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

/// 处理密钥生成命令
///
/// 参数0: 密钥标识符（输入）
/// 参数1: 生成的密钥数据（输出，包含认证数据和加密密钥）
fn handle_key_generate(cmd_id: u32, params: &mut Parameters) -> Result<()> {
    // 期望的参数类型：输入内存引用 + 输出内存引用
    let exp_pt = TEE_PARAM_TYPES(
        ParamType::MemrefInput as u32,
        ParamType::MemrefOutput as u32,
        ParamType::None as u32,
        ParamType::None as u32,
    );

    let pt = TEE_PARAM_TYPES(
        params.0.param_type as u32,
        params.1.param_type as u32,
        params.2.param_type as u32,
        params.3.param_type as u32,
    );

    expect_param_types(cmd_id, pt, exp_pt)?;

    let key_id_vec = params.0.param.data.clone();
    let (key_auth, key, iv) = cc_generate_box_key()?;
    let key_id_str = match String::from_utf8(key_id_vec) {
        Ok(s) => s,
        Err(_) => return Err(Error::BadParameters),
    };

    // 合并认证数据和加密密钥
    let mut key_save = vec![0u8; key_auth.len() + key.len()];
    key_save[..key_auth.len()].copy_from_slice(&key_auth);
    key_save[key_auth.len()..].copy_from_slice(&key);

    cc_save_box_key(&key_save, &iv, &key_id_str)?;

    // 清空输入参数，防止信息泄露
    params.0.param.data.clear();
    params.1.param.data.clear();
    params.1.param.data.extend_from_slice(&key_save);

    Ok(())
}

/// 处理密钥销毁命令
///
/// 参数0: 密钥标识符（输入）
/// 参数1: 操作结果（输出，1表示成功）
fn handle_key_destroy(cmd_id: u32, params: &mut Parameters) -> Result<()> {
    let exp_pt = TEE_PARAM_TYPES(
        ParamType::MemrefInput as u32,
        ParamType::ValueOutput as u32,
        ParamType::None as u32,
        ParamType::None as u32,
    );

    let pt = TEE_PARAM_TYPES(
        params.0.param_type as u32,
        params.1.param_type as u32,
        params.2.param_type as u32,
        params.3.param_type as u32,
    );

    expect_param_types(cmd_id, pt, exp_pt)?;

    let key_id_vec = params.0.param.data.clone();
    let key_id_str = match String::from_utf8(key_id_vec) {
        Ok(s) => s,
        Err(_) => return Err(Error::BadParameters),
    };

    cc_destroy_box_key(&key_id_str)?;

    params.0.param.data.clear();
    params.1.param.values.a = 1;

    Ok(())
}

/// 处理密钥存在性检查命令
///
/// 参数0: 密钥标识符（输入）
/// 参数1: 存在性结果（输出，1表示存在，0表示不存在）
fn handle_key_is_exist(cmd_id: u32, params: &mut Parameters) -> Result<()> {
    let exp_pt = TEE_PARAM_TYPES(
        ParamType::MemrefInput as u32,
        ParamType::ValueOutput as u32,
        ParamType::None as u32,
        ParamType::None as u32,
    );

    let pt = TEE_PARAM_TYPES(
        params.0.param_type as u32,
        params.1.param_type as u32,
        params.2.param_type as u32,
        params.3.param_type as u32,
    );

    expect_param_types(cmd_id, pt, exp_pt)?;

    let key_id_vec = params.0.param.data.clone();
    let key_id_str = match String::from_utf8(key_id_vec) {
        Ok(s) => s,
        Err(_) => return Err(Error::BadParameters),
    };

    let is_exist: u32 = if cc_box_key_is_exist(&key_id_str)? {
        1
    } else {
        0
    };

    params.0.param.data.clear();
    params.1.param.values.a = is_exist;

    Ok(())
}

/// 处理密钥验证命令
///
/// 参数0: 密钥标识符（输入）
/// 参数1: 待验证的密钥认证数据（输入）
/// 参数2: 验证结果（输出，1表示验证通过，0表示失败）
fn handle_verify_key(cmd_id: u32, params: &mut Parameters) -> Result<()> {
    let exp_pt = TEE_PARAM_TYPES(
        ParamType::MemrefInput as u32,
        ParamType::MemrefInput as u32,
        ParamType::ValueOutput as u32,
        ParamType::None as u32,
    );

    let pt = TEE_PARAM_TYPES(
        params.0.param_type as u32,
        params.1.param_type as u32,
        params.2.param_type as u32,
        params.3.param_type as u32,
    );

    expect_param_types(cmd_id, pt, exp_pt)?;

    let key_id_vec = params.0.param.data.clone();
    let key_id_str = match String::from_utf8(key_id_vec) {
        Ok(s) => s,
        Err(_) => return Err(Error::BadParameters),
    };
    let key_auth_vec = params.1.param.data.clone();

    let ret: u32 = if cc_verify_box_key(&key_auth_vec, &key_id_str)? {
        1
    } else {
        0
    };

    params.0.param.data.clear();
    params.1.param.data.clear();
    params.2.param.data.clear();
    params.2.param.values.a = ret;

    Ok(())
}

/// 处理数据加密命令
///
/// 参数0: 密钥标识符（输入）
/// 参数1: 明文数据（输入）
/// 参数2: 密文数据（输出）
fn handle_encrypt(cmd_id: u32, params: &mut Parameters) -> Result<()> {
    let exp_pt = TEE_PARAM_TYPES(
        ParamType::MemrefInput as u32,
        ParamType::MemrefInput as u32,
        ParamType::MemrefOutput as u32,
        ParamType::None as u32,
    );

    let pt = TEE_PARAM_TYPES(
        params.0.param_type as u32,
        params.1.param_type as u32,
        params.2.param_type as u32,
        params.3.param_type as u32,
    );

    expect_param_types(cmd_id, pt, exp_pt)?;

    let key_id_vec = params.0.param.data.clone();
    let key_id_str = match String::from_utf8(key_id_vec) {
        Ok(s) => s,
        Err(_) => return Err(Error::BadParameters),
    };
    let plain_data = params.1.param.data.clone();

    let (key_size, iv_size, _) = get_box_cipher_info().map_err(|e| {
        trace_println!("TA KyBox: failed to get box cipher info: {}", e);
        Error::Generic
    })?;
    let mut key = vec![0u8; key_size];
    let mut iv = vec![0u8; iv_size];

    cc_get_box_key(&mut key, &mut iv, &key_id_str)?;

    let cipherdata = encrypt_box_data(&key, &iv, &plain_data).map_err(|e| {
        trace_println!("TA KyBox: failed to encrypt_box_data: {}", e);
        Error::Generic
    })?;

    params.0.param.data.clear();
    params.1.param.data.clear();
    params.2.param.data.clear();
    params.2.param.data.extend_from_slice(&cipherdata);

    Ok(())
}

/// 处理数据解密命令
///
/// 参数0: 密钥标识符（输入）
/// 参数1: 密钥认证数据（输入，用于验证调用者权限）
/// 参数2: 密文数据（输入）
/// 参数3: 明文数据（输出）
fn handle_decrypt(cmd_id: u32, params: &mut Parameters) -> Result<()> {
    let exp_pt = TEE_PARAM_TYPES(
        ParamType::MemrefInput.into(),
        ParamType::MemrefInput.into(),
        ParamType::MemrefInput.into(),
        ParamType::MemrefOutput.into(),
    );

    let pt = TEE_PARAM_TYPES(
        params.0.param_type.into(),
        params.1.param_type.into(),
        params.2.param_type.into(),
        params.3.param_type.into(),
    );

    expect_param_types(cmd_id, pt, exp_pt)?;

    let key_id_vec = params.0.param.data.clone();
    let key_id_str = match String::from_utf8(key_id_vec) {
        Ok(s) => s,
        Err(_) => return Err(Error::BadParameters),
    };
    let key_auth = params.1.param.data.clone();

    if !(cc_verify_box_key(&key_auth, &key_id_str)?) {
        trace_println!("TA KyBox: key auth 验证失败");
        return Err(Error::AccessDenied);
    }

    let cipherdata = params.2.param.data.clone();
    let (key_size, iv_size, _) = get_box_cipher_info().map_err(|e| {
        trace_println!("TA KyBox: failed to get box cipher info: {}", e);
        Error::Generic
    })?;
    let mut key = vec![0u8; key_size];
    let mut iv = vec![0u8; iv_size];

    cc_get_box_key(&mut key, &mut iv, &key_id_str)?;

    let plaindata = decrypt_box_data(&key, &iv, &cipherdata).map_err(|e| {
        trace_println!("TA KyBox: failed to decrypt_box_data: {}", e);
        Error::Generic
    })?;

    params.0.param.data.clear();
    params.1.param.data.clear();
    params.2.param.data.clear();
    params.3.param.data.clear();
    params.3.param.data.extend_from_slice(&plaindata);

    Ok(())
}

struct TA;

impl TrustedApplication for TA {
    type SessionContext = ();

    fn create(&self) -> Result<()> {
        trace_println!("[+] TA open session");
        Ok(())
    }

    fn open_session(&self, _params: &mut Parameters) -> Result<()> {
        trace_println!("[+] TA open session");
        Ok(())
    }

    fn close_session(&self, _ctx: &mut Self::SessionContext) -> Result<()> {
        trace_println!("[+] TA close session");
        Ok(())
    }

    fn destroy(&self) -> Result<()> {
        trace_println!("[+] TA destroy");
        Ok(())
    }

    fn invoke_command(
        &self,
        cmd_id: u32,
        params: &mut Parameters,
        _ctx: &mut Self::SessionContext,
    ) -> Result<()> {
        trace_println!("[+] TA invoke command");
        match Command::from(cmd_id) {
            Command::GenerateBoxKey => Ok(handle_key_generate(cmd_id, params)?),
            Command::DestroyBoxKey => Ok(handle_key_destroy(cmd_id, params)?),
            Command::BoxKeyIsExist => Ok(handle_key_is_exist(cmd_id, params)?),
            Command::Encrypt => Ok(handle_encrypt(cmd_id, params)?),
            Command::Decrypt => Ok(handle_decrypt(cmd_id, params)?),
            Command::VerifyBoxKey => Ok(handle_verify_key(cmd_id, params)?),
            _ => Err(Error::NotSupported),
        }
    }
}

fn main() {
    let ta = TA;
    let mut ta_manager = TAManager::new(ta, UUID);
    #[allow(clippy::expect_used)]
    ta_manager.run_ta().expect("Failed to run TA");
}
