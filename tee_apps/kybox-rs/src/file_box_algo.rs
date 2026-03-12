use mbedtls::Error;
use mbedtls::cipher::raw::CipherPadding;
use mbedtls::cipher::{Cipher, Encryption, Traditional, Type, raw};

const SM4_KEY_BIT_SIZE: usize = 128;
const BOX_CIPHER_ID: raw::CipherId = raw::CipherId::SM4;
const BOX_CIPHER_MODE: raw::CipherMode = raw::CipherMode::CBC;
const BOX_KEY_SIZE: usize = SM4_KEY_BIT_SIZE / 8; // AES-128 / SM4

/// box 加密数据
///
/// # 参数
/// - `key`: 加密密钥字节切片
/// - `iv`: 初始化向量字节切片
/// - `plain_data`: 待加密的明文数据字节切片
///
/// # 返回
/// - `Ok(Vec<u8>)`: 加密后的密文数据
/// - `Err(mbedtls::Error)`: 加密过程中发生的错误
pub fn encrypt_box_data(
    key: &[u8],
    iv: &[u8],
    plain_data: &[u8],
) -> Result<Vec<u8>, mbedtls::Error> {
    // 创建 SM4 加密器
    let aes =
        Cipher::<_, Traditional, _>::new(BOX_CIPHER_ID, BOX_CIPHER_MODE, (key.len() * 8) as _)?;

    // 执行加密
    let mut encrypted_data = vec![0u8; plain_data.len() + aes.block_size()];
    let aes_c = aes.set_key_iv(key, iv)?;

    let (encrypted_len, _) = aes_c.encrypt(plain_data, &mut encrypted_data)?;

    // 截取实际加密长度的数据
    encrypted_data.truncate(encrypted_len);

    Ok(encrypted_data)
}

/// 解密 box 加密数据
///
/// # 参数
/// - `key`: 解密密钥字节切片
/// - `iv`: 初始化向量字节切片
/// - `enc_data`: 待解密的密文数据字节切片
///
/// # 返回
/// - `Ok(Vec<u8>)`: 解密后的明文数据
/// - `Err(mbedtls::Error)`: 解密过程中发生的错误
pub fn decrypt_box_data(key: &[u8], iv: &[u8], enc_data: &[u8]) -> Result<Vec<u8>, mbedtls::Error> {
    // 创建 SM4 解密器
    let aes =
        Cipher::<_, Traditional, _>::new(BOX_CIPHER_ID, BOX_CIPHER_MODE, (key.len() * 8) as _)?;
    let mut decrypted_data = vec![0u8; enc_data.len() + aes.block_size()];
    // 执行解密
    let aes_c = aes.set_key_iv(key, iv)?;
    let (decrypted_len, _) = aes_c.decrypt(enc_data, &mut decrypted_data)?;
    // 截取实际解密长度的数据
    decrypted_data.truncate(decrypted_len);

    Ok(decrypted_data)
}

/// 获取 box 加密算法相关信息
///
/// # 返回
/// - `Ok((key_size, iv_size, block_size))`: 分别为密钥长度、IV长度、分组长度
/// - `Err(mbedtls::Error)`: 获取过程中发生的错误
pub fn get_box_cipher_info() -> Result<(usize, usize, usize), mbedtls::Error> {
    let aes = Cipher::<Encryption, Traditional, _>::new(
        BOX_CIPHER_ID,
        BOX_CIPHER_MODE,
        (BOX_KEY_SIZE * 8) as _,
    )?;

    Ok((BOX_KEY_SIZE, aes.iv_size(), aes.block_size()))
}
