// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{string::String, vec};

mod file_box_algo;
mod file_box_key;
mod proto;

pub use file_box_algo::get_box_cipher_info;
use file_box_algo::{decrypt_box_data, encrypt_box_data};
use file_box_key::{
    cc_box_key_is_exist, cc_destroy_box_key, cc_generate_box_key, cc_get_box_key, cc_save_box_key,
    cc_verify_box_key,
};
use xtee_utee::{
    error::{Error, Result},
    ta_manager::{TAManager, TrustedApplication},
    tee_api_defines::TEE_PARAM_TYPES,
    trace_println,
    wire::{ParamType, Parameters},
};

use crate::proto::{Command, UUID};

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
