//! PAT (Program Association Table) section 构造。

use super::crc32::mpeg_crc32;

/// 构造 PAT section（不含 TS 包头、不含 pointer_field）。
/// program 1 → PMT PID。
pub fn build_pat_section(pmt_pid: u16) -> Vec<u8> {
    let mut s = Vec::new();
    s.push(0x00); // table_id = PAT
                  // section_syntax_indicator=1, '0', reserved=11, section_length(12) 占位
                  // 先放占位，最后回填
    s.push(0xB0); // 1011 0000，高 4 位含 section_length 高位（先 0）
    s.push(0x00); // section_length 低 8 位（回填）
    s.extend_from_slice(&0x0001u16.to_be_bytes()); // transport_stream_id
    s.push(0xC1); // reserved=11, version=00000, current_next=1
    s.push(0x00); // section_number
    s.push(0x00); // last_section_number
                  // program loop
    s.extend_from_slice(&0x0001u16.to_be_bytes()); // program_number
    s.extend_from_slice(&(0xE000 | (pmt_pid & 0x1FFF)).to_be_bytes()); // reserved=111 + PMT PID

    finish_section(&mut s);
    s
}

/// 回填 section_length 并追加 CRC32。
/// section_length = 从该字段之后到 CRC 末尾的字节数。
pub(crate) fn finish_section(s: &mut Vec<u8>) {
    // section_length 覆盖：byte3..end + CRC(4)
    let section_length = (s.len() - 3) + 4;
    s[1] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
    s[2] = (section_length & 0xFF) as u8;
    let crc = mpeg_crc32(s);
    s.extend_from_slice(&crc.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pat_well_formed() {
        let pat = build_pat_section(0x0100);
        assert_eq!(pat[0], 0x00); // table_id
                                  // 最小 PAT 应为 16 字节 (含4字节CRC)
        assert_eq!(pat.len(), 16);
        // CRC 自洽：对整段(去CRC)重算应相等
        let crc = mpeg_crc32(&pat[..pat.len() - 4]);
        let stored = u32::from_be_bytes([
            pat[pat.len() - 4],
            pat[pat.len() - 3],
            pat[pat.len() - 2],
            pat[pat.len() - 1],
        ]);
        assert_eq!(crc, stored);
    }
}
