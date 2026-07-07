//! CENC/CBCS sample decryption.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, KeyInit, KeyIvInit, StreamCipher};
use ctr::Ctr128BE;

use crate::mp4::sample::{SampleInfo, SubSample};

type Aes128Ctr = Ctr128BE<aes::Aes128>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncryptionMode {
    CencCtr,
    Cbcs {
        crypt_byte_block: u8,
        skip_byte_block: u8,
    },
}

/// 持有 16 字节 key 的解密器。
#[derive(Clone)]
pub struct Decryptor {
    key: [u8; 16],
    /// constant IV（音频 tenc iv_size=0 时使用）。
    constant_iv: Option<[u8; 16]>,
    mode: EncryptionMode,
}

impl Decryptor {
    pub fn new(key: [u8; 16]) -> Self {
        Self {
            key,
            constant_iv: None,
            mode: EncryptionMode::CencCtr,
        }
    }

    pub fn with_constant_iv(key: [u8; 16], iv: [u8; 16]) -> Self {
        Self {
            key,
            constant_iv: Some(iv),
            mode: EncryptionMode::CencCtr,
        }
    }

    pub fn new_cbcs(key: [u8; 16], crypt_byte_block: u8, skip_byte_block: u8) -> Self {
        Self {
            key,
            constant_iv: None,
            mode: EncryptionMode::Cbcs {
                crypt_byte_block,
                skip_byte_block,
            },
        }
    }

    pub fn cbcs_with_constant_iv(
        key: [u8; 16],
        iv: [u8; 16],
        crypt_byte_block: u8,
        skip_byte_block: u8,
    ) -> Self {
        Self {
            key,
            constant_iv: Some(iv),
            mode: EncryptionMode::Cbcs {
                crypt_byte_block,
                skip_byte_block,
            },
        }
    }

    /// 把 per-sample IV（8 或 16 字节）补齐到 16 字节 counter（CTR 初值）。
    fn make_counter(&self, iv: &[u8]) -> [u8; 16] {
        let mut c = [0u8; 16];
        if iv.is_empty() {
            if let Some(civ) = self.constant_iv {
                return civ;
            }
        } else {
            let n = iv.len().min(16);
            c[..n].copy_from_slice(&iv[..n]);
        }
        c
    }

    /// 原地解密一个样本。`data` 是该样本的密文（可变），按 `info` 的 subsample 切分。
    /// 视频 subsample 模式：只解 encrypted 段；音频整样本：全解。
    pub fn decrypt_sample(&self, data: &mut [u8], info: &SampleInfo) {
        match self.mode {
            EncryptionMode::CencCtr => self.decrypt_ctr_sample(data, info),
            EncryptionMode::Cbcs {
                crypt_byte_block,
                skip_byte_block,
            } => self.decrypt_cbcs_sample(data, info, crypt_byte_block, skip_byte_block),
        }
    }

    fn decrypt_ctr_sample(&self, data: &mut [u8], info: &SampleInfo) {
        let counter = self.make_counter(&info.iv);
        let mut cipher = Aes128Ctr::new(&self.key.into(), &counter.into());

        if info.subsamples.is_empty() {
            // 整样本加密
            cipher.apply_keystream(data);
        } else {
            // subsample：clear 段跳过（但 CTR 计数器不为 clear 段前进——
            // CENC 规定 keystream 只在 encrypted 字节上消耗，clear 段不推进计数器）。
            let mut pos = 0usize;
            for &SubSample { clear, encrypted } in &info.subsamples {
                let clear = clear as usize;
                let enc = encrypted as usize;
                pos += clear; // 跳过 clear
                if enc > 0 {
                    let end = (pos + enc).min(data.len());
                    cipher.apply_keystream(&mut data[pos..end]);
                    pos = end;
                }
            }
        }
    }

    fn decrypt_cbcs_sample(
        &self,
        data: &mut [u8],
        info: &SampleInfo,
        crypt_byte_block: u8,
        skip_byte_block: u8,
    ) {
        let mut iv = self.make_counter(&info.iv);
        if info.subsamples.is_empty() {
            self.decrypt_cbcs_region(data, &mut iv, crypt_byte_block, skip_byte_block);
            return;
        }

        let mut pos = 0usize;
        for &SubSample { clear, encrypted } in &info.subsamples {
            pos = pos.saturating_add(clear as usize);
            let enc = encrypted as usize;
            if enc > 0 && pos < data.len() {
                let end = (pos + enc).min(data.len());
                self.decrypt_cbcs_region(
                    &mut data[pos..end],
                    &mut iv,
                    crypt_byte_block,
                    skip_byte_block,
                );
                pos = end;
            }
        }
    }

    fn decrypt_cbcs_region(
        &self,
        data: &mut [u8],
        iv: &mut [u8; 16],
        crypt_byte_block: u8,
        skip_byte_block: u8,
    ) {
        let blocks = data.len() / 16;
        if blocks == 0 {
            return;
        }

        let crypt = crypt_byte_block as usize;
        let skip = skip_byte_block as usize;
        if crypt == 0 && skip == 0 {
            self.decrypt_cbc_blocks(&mut data[..blocks * 16], iv);
            return;
        }

        let mut block = 0usize;
        while block < blocks {
            let ncrypt = (blocks - block).min(crypt);
            if ncrypt > 0 {
                let start = block * 16;
                let end = start + ncrypt * 16;
                self.decrypt_cbc_blocks(&mut data[start..end], iv);
                block += ncrypt;
            }

            let nskip = (blocks - block).min(skip);
            block += nskip;

            if ncrypt == 0 && nskip == 0 {
                break;
            }
        }
    }

    fn decrypt_cbc_blocks(&self, data: &mut [u8], iv: &mut [u8; 16]) {
        let cipher = aes::Aes128::new((&self.key).into());
        for chunk in data.chunks_exact_mut(16) {
            let mut ciphertext = [0u8; 16];
            ciphertext.copy_from_slice(chunk);
            let mut block = GenericArray::clone_from_slice(chunk);
            cipher.decrypt_block(&mut block);
            for i in 0..16 {
                chunk[i] = block[i] ^ iv[i];
            }
            *iv = ciphertext;
        }
    }

    /// 便捷：解密整段中所有样本（原地修改 mdat 副本）。
    pub fn decrypt_segment(&self, mdat: &mut [u8], samples: &[SampleInfo]) {
        for s in samples {
            let (a, b) = s.data_range;
            let b = b.min(mdat.len());
            if a < b {
                self.decrypt_sample(&mut mdat[a..b], s);
            }
        }
    }
}

/// 一组 KID→KEY 映射，从多行 "KID:KEY" 文本解析。按 KID 查 key。
#[derive(Clone, Default)]
pub struct KeyStore {
    map: std::collections::HashMap<String, [u8; 16]>,
    /// 没有冒号的裸 key（无法按 KID 匹配时的回退）。
    bare: Vec<[u8; 16]>,
}

impl KeyStore {
    /// 从多行文本解析，每行一个 "KID:KEY" 或纯 "KEY"。空行/空白忽略。
    pub fn parse(text: &str) -> Self {
        let mut store = KeyStore::default();
        store.extend_from_text(text);
        store
    }

    pub fn extend_from_text(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(key) = parse_key_hex(line) {
                if let Some(kid) = parse_kid_hex(line) {
                    self.map.insert(normalize_kid(&kid), key);
                } else {
                    self.bare.push(key);
                }
            }
        }
    }

    pub fn merge(&mut self, other: KeyStore) {
        self.map.extend(other.map);
        self.bare.extend(other.bare);
    }

    /// 按 KID 查 key；找不到则回退到第一个裸 key（若有）。
    pub fn get(&self, kid: &str) -> Option<[u8; 16]> {
        self.map
            .get(&normalize_kid(kid))
            .copied()
            .or_else(|| self.bare.first().copied())
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty() && self.bare.is_empty()
    }

    pub fn has_kid(&self, kid: &str) -> bool {
        self.map.contains_key(&normalize_kid(kid))
    }

    pub fn has_bare_key(&self) -> bool {
        !self.bare.is_empty()
    }
}

fn normalize_kid(kid: &str) -> String {
    kid.trim().to_lowercase().replace('-', "")
}

/// 解析 "KID:KEY" 或纯 "KEY" 形式的 hex 字符串为 16 字节 key。
pub fn parse_key_hex(s: &str) -> Option<[u8; 16]> {
    let key_part = s.rsplit(':').next().unwrap_or(s).trim();
    let bytes = hex_decode(key_part)?;
    if bytes.len() != 16 {
        return None;
    }
    let mut k = [0u8; 16];
    k.copy_from_slice(&bytes);
    Some(k)
}

/// 解析 "KID:KEY" 的 KID 部分。
pub fn parse_kid_hex(s: &str) -> Option<String> {
    if let Some((kid, _)) = s.split_once(':') {
        Some(kid.trim().to_lowercase().replace('-', ""))
    } else {
        None
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncrypt;

    #[test]
    fn parse_keys() {
        let k = parse_key_hex("00112233445566778899aabbccddeeff:ffeeddccbbaa99887766554433221100")
            .unwrap();
        assert_eq!(k[0], 0xff);
        assert_eq!(k[15], 0x00);
        let kid =
            parse_kid_hex("00112233445566778899aabbccddeeff:ffeeddccbbaa99887766554433221100")
                .unwrap();
        assert_eq!(kid, "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn ctr_seek_unused_ok() {
        // 仅确保类型可构造
        let d = Decryptor::new([0u8; 16]);
        let mut buf = vec![1u8, 2, 3, 4];
        let info = SampleInfo {
            data_range: (0, 4),
            duration: 0,
            cts_offset: 0,
            iv: vec![0u8; 8],
            subsamples: vec![],
        };
        d.decrypt_sample(&mut buf, &info);
        // key=0,iv=0 的 CTR keystream 非平凡，buf 应被改动
        assert_ne!(buf, vec![1u8, 2, 3, 4]);
    }

    #[test]
    fn cbcs_full_cbc_decrypts_full_blocks_and_leaves_tail() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let plain = (0u8..34).collect::<Vec<_>>();
        let mut encrypted = plain.clone();
        encrypt_cbc_blocks(&key, &iv, &mut encrypted[..32]);

        let d = Decryptor::cbcs_with_constant_iv(key, iv, 0, 0);
        let info = SampleInfo {
            data_range: (0, encrypted.len()),
            duration: 0,
            cts_offset: 0,
            iv: Vec::new(),
            subsamples: Vec::new(),
        };
        d.decrypt_sample(&mut encrypted, &info);
        assert_eq!(encrypted, plain);
    }

    #[test]
    fn cbcs_pattern_skips_clear_blocks_without_chaining_them() {
        let key = [0x33u8; 16];
        let iv = [0x44u8; 16];
        let plain = (0u8..64).collect::<Vec<_>>();
        let mut encrypted = plain.clone();

        let mut state = iv;
        encrypt_cbc_blocks_stateful(&key, &mut state, &mut encrypted[0..16]);
        encrypt_cbc_blocks_stateful(&key, &mut state, &mut encrypted[32..48]);

        let d = Decryptor::new_cbcs(key, 1, 1);
        let info = SampleInfo {
            data_range: (0, encrypted.len()),
            duration: 0,
            cts_offset: 0,
            iv: iv.to_vec(),
            subsamples: Vec::new(),
        };
        d.decrypt_sample(&mut encrypted, &info);
        assert_eq!(encrypted, plain);
    }

    fn encrypt_cbc_blocks(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
        let mut state = *iv;
        encrypt_cbc_blocks_stateful(key, &mut state, data);
    }

    fn encrypt_cbc_blocks_stateful(key: &[u8; 16], iv: &mut [u8; 16], data: &mut [u8]) {
        let cipher = aes::Aes128::new(key.into());
        for chunk in data.chunks_exact_mut(16) {
            for i in 0..16 {
                chunk[i] ^= iv[i];
            }
            let mut block = GenericArray::clone_from_slice(chunk);
            cipher.encrypt_block(&mut block);
            chunk.copy_from_slice(&block);
            iv.copy_from_slice(chunk);
        }
    }
}
