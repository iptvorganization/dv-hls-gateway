//! HLS AES-128-CBC segment decryption.

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};

/// HLS AES-128 default IV: 128-bit big-endian media sequence number.
pub fn iv_from_media_sequence(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

/// Decrypt a full-segment HLS `METHOD=AES-128` payload.
///
/// Reference binary behavior matches AES-128-CBC with PKCS#7 unpadding and a
/// strict 16-byte ciphertext block multiple.
pub fn decrypt_aes128_cbc_pkcs7(
    ciphertext: &[u8],
    key: [u8; 16],
    iv: [u8; 16],
) -> crate::Result<Vec<u8>> {
    if ciphertext.is_empty() {
        return Ok(Vec::new());
    }
    if ciphertext.len() % 16 != 0 {
        return Err(anyhow::anyhow!(
            "AES-128-CBC ciphertext length {} is not a multiple of 16",
            ciphertext.len()
        ));
    }

    let cipher = aes::Aes128::new(GenericArray::from_slice(&key));
    let mut out = ciphertext.to_vec();
    let mut prev = iv;

    for block in out.chunks_exact_mut(16) {
        let cur: [u8; 16] = block.try_into().expect("chunks_exact(16)");
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        prev = cur;
    }

    let pad = *out.last().unwrap() as usize;
    if pad == 0 || pad > 16 || pad > out.len() {
        return Err(anyhow::anyhow!("AES-128-CBC unpad failed"));
    }
    if !out[out.len() - pad..].iter().all(|&b| b as usize == pad) {
        return Err(anyhow::anyhow!("AES-128-CBC unpad failed"));
    }
    out.truncate(out.len() - pad);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncrypt;

    fn encrypt_for_test(plaintext: &[u8], key: [u8; 16], iv: [u8; 16]) -> Vec<u8> {
        let mut data = plaintext.to_vec();
        let pad = 16 - (data.len() % 16);
        data.extend(std::iter::repeat(pad as u8).take(pad));

        let cipher = aes::Aes128::new(GenericArray::from_slice(&key));
        let mut prev = iv;
        for block in data.chunks_exact_mut(16) {
            for (b, p) in block.iter_mut().zip(prev) {
                *b ^= p;
            }
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
            prev = block.try_into().expect("chunks_exact(16)");
        }
        data
    }

    #[test]
    fn default_iv_is_big_endian_media_sequence() {
        let iv = iv_from_media_sequence(0x0102_0304_0506_0708);
        assert_eq!(&iv[..8], &[0u8; 8]);
        assert_eq!(&iv[8..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn decrypt_roundtrip_removes_pkcs7_padding() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let plain = b"two ts packets worth of bytes";
        let enc = encrypt_for_test(plain, key, iv);
        let dec = decrypt_aes128_cbc_pkcs7(&enc, key, iv).unwrap();
        assert_eq!(dec, plain);
    }

    #[test]
    fn rejects_non_block_multiple() {
        let err = decrypt_aes128_cbc_pkcs7(&[1, 2, 3], [0u8; 16], [0u8; 16]).unwrap_err();
        assert!(err.to_string().contains("multiple of 16"));
    }

    #[test]
    fn rejects_bad_padding() {
        let err = decrypt_aes128_cbc_pkcs7(&[0u8; 16], [0u8; 16], [0u8; 16]).unwrap_err();
        assert!(err.to_string().contains("unpad"));
    }
}
