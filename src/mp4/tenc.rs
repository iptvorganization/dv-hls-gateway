//! `tenc` (TrackEncryption) box 解析 → 读取 default_KID（用于按 KID 匹配 key）。
//!
//! 实测布局（version 0）：
//! ```text
//! version(1) flags(3) reserved(1) reserved(1)
//! default_isProtected(1) default_Per_Sample_IV_Size(1) default_KID(16)
//! ```
//! version 1 时第 5 字节是 (crypt_byte_block<<4 | skip_byte_block)，其余相同。

use super::boxes::{find_box, CONTAINERS};

/// tenc 解析结果。
#[derive(Debug, Clone)]
pub struct TrackEncryption {
    pub is_protected: bool,
    pub iv_size: u8,
    /// `schm` scheme type, e.g. `cenc` or `cbcs`.
    pub scheme: Option<String>,
    /// CENC pattern encryption crypt byte blocks. `cbcs` commonly uses 1.
    pub crypt_byte_block: u8,
    /// CENC pattern encryption skip byte blocks. `cbcs` commonly uses 9.
    pub skip_byte_block: u8,
    /// 32 个十六进制小写字符的 KID。
    pub kid: String,
    /// constant IV（iv_size==0 时存在），16 字节。
    pub constant_iv: Option<[u8; 16]>,
}

impl TrackEncryption {
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 24 {
            return None;
        }
        // version(1) flags(3) reserved(1) reserved/cryptskip(1) isProtected(1) iv_size(1) KID(16)
        let version = payload[0];
        let pattern = if version > 0 { payload[5] } else { 0 };
        let mut p = 6;
        let is_protected = payload[p] != 0;
        p += 1;
        let iv_size = payload[p];
        p += 1;
        let kid_bytes = &payload[p..p + 16];
        p += 16;
        let kid = kid_bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();

        let constant_iv = if iv_size == 0 && p < payload.len() {
            let civ_size = payload[p] as usize;
            p += 1;
            if civ_size == 16 && p + 16 <= payload.len() {
                let mut iv = [0u8; 16];
                iv.copy_from_slice(&payload[p..p + 16]);
                Some(iv)
            } else {
                None
            }
        } else {
            None
        };

        Some(Self {
            is_protected,
            iv_size,
            scheme: None,
            crypt_byte_block: pattern >> 4,
            skip_byte_block: pattern & 0x0f,
            kid,
            constant_iv,
        })
    }

    /// 在 init 段里查找 tenc 并解析。
    pub fn find_in_init(init: &[u8]) -> Option<Self> {
        let p = find_box(init, b"tenc", CONTAINERS).or_else(|| scan_anywhere(init, b"tenc"))?;
        let mut tenc = Self::parse(p)?;
        tenc.scheme = find_scheme_type(init);
        Some(tenc)
    }

    pub fn is_cbcs(&self) -> bool {
        self.scheme.as_deref() == Some("cbcs")
    }
}

fn find_scheme_type(init: &[u8]) -> Option<String> {
    let p = find_box(init, b"schm", CONTAINERS).or_else(|| scan_anywhere(init, b"schm"))?;
    if p.len() < 8 {
        return None;
    }
    Some(String::from_utf8_lossy(&p[4..8]).to_string())
}

fn scan_anywhere<'a>(data: &'a [u8], target: &[u8; 4]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i + 8 <= data.len() {
        if &data[i + 4..i + 8] == target {
            let size =
                u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
            if size >= 8 && i + size <= data.len() {
                return Some(&data[i + 8..i + size]);
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_real_tenc() {
        let payload = hex(b"000000000000010800112233445566778899aabbccddeeff");
        let t = TrackEncryption::parse(&payload).unwrap();
        assert!(t.is_protected);
        assert_eq!(t.iv_size, 8);
        assert_eq!(t.kid, "00112233445566778899aabbccddeeff");
        assert_eq!(t.crypt_byte_block, 0);
        assert_eq!(t.skip_byte_block, 0);
        assert!(t.constant_iv.is_none());
    }

    #[test]
    fn parse_cbcs_tenc_v1_pattern_and_constant_iv() {
        let payload = hex(
            b"0100000000190100112233445566778899aabbccddeeff10100102030405060708090a0b0c0d0e0f10",
        );
        let t = TrackEncryption::parse(&payload).unwrap();
        assert!(t.is_protected);
        assert_eq!(t.iv_size, 0);
        assert_eq!(t.crypt_byte_block, 1);
        assert_eq!(t.skip_byte_block, 9);
        assert_eq!(t.kid, "112233445566778899aabbccddeeff10");
        assert_eq!(
            t.constant_iv.unwrap(),
            hex(b"0102030405060708090a0b0c0d0e0f10")[..]
        );
    }

    fn hex(s: &[u8]) -> Vec<u8> {
        let s = std::str::from_utf8(s).unwrap();
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }
}
