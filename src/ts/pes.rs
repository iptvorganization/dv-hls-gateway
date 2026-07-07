//! PES 打包：把一个访问单元的 ES 字节加上 PES header（含 PTS/DTS）。

use super::bitwriter::BitWriter;

/// 构造一个 PES packet（视频不定长，PES_packet_length=0）。
/// - `stream_id`: 0xE0(video) / 0xC0(audio)
/// - `pts90`/`dts90`: 90kHz 时间戳；若 `dts90==None` 则只写 PTS。
/// - `payload`: ES 数据（视频=Annex-B AU，音频=帧字节）。
pub fn build_pes(stream_id: u8, pts90: u64, dts90: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 19);
    out.extend_from_slice(&[0x00, 0x00, 0x01]); // packet_start_code_prefix
    out.push(stream_id);

    // optional PES header
    let mut hdr = BitWriter::new();
    hdr.bits(0b10, 2); // '10'
    hdr.bits(0, 2); // scrambling
    hdr.bit(false); // priority
    hdr.bit(true); // data_alignment_indicator = 1（AU 对齐）
    hdr.bit(false); // copyright
    hdr.bit(false); // original_or_copy
    let pts_dts_flags = if dts90.is_some() { 0b11u32 } else { 0b10 };
    hdr.bits(pts_dts_flags, 2);
    hdr.bits(0, 6); // ESCR/ES_rate/DSM/add_copy/CRC/ext flags = 0
    let header_data_len = if dts90.is_some() { 10u8 } else { 5 };
    hdr.u8(header_data_len);

    // PTS
    write_timestamp(
        &mut hdr,
        if dts90.is_some() { 0b0011 } else { 0b0010 },
        pts90,
    );
    if let Some(dts) = dts90 {
        write_timestamp(&mut hdr, 0b0001, dts);
    }

    let hdr = hdr.into_bytes();
    // PES_packet_length：视频流允许填 0 表示不定长；音频流写实际长度，播放器/探测器
    // 对独立 HLS TS 段会更容易锁定 codec 参数。
    let pes_packet_length = if (0xE0..=0xEF).contains(&stream_id) {
        0
    } else {
        (hdr.len() + payload.len()).min(u16::MAX as usize) as u16
    };
    out.extend_from_slice(&pes_packet_length.to_be_bytes());
    out.extend_from_slice(&hdr);
    out.extend_from_slice(payload);
    out
}

/// 写 5 字节 PTS/DTS 时间戳。`prefix` 是高 4 位标记（PTS-only=0010, PTS+DTS的PTS=0011, DTS=0001）。
fn write_timestamp(w: &mut BitWriter, prefix: u32, ts: u64) {
    let ts = ts & 0x1_FFFF_FFFF; // 33 bit
    w.bits(prefix, 4);
    w.bits(((ts >> 30) & 0x7) as u32, 3);
    w.bit(true); // marker
    w.bits(((ts >> 15) & 0x7FFF) as u32, 15);
    w.bit(true); // marker
    w.bits((ts & 0x7FFF) as u32, 15);
    w.bit(true); // marker
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pes_header_pts_dts() {
        let pes = build_pes(0xE0, 90000, Some(86247), &[0xAA, 0xBB]);
        assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01]);
        assert_eq!(pes[3], 0xE0);
        // PES_packet_length=0
        assert_eq!(&pes[4..6], &[0x00, 0x00]);
        // optional header 第一字节高2位=10
        assert_eq!(pes[6] & 0xC0, 0x80);
        // data_alignment bit (pes[6] bit2)
        assert_eq!(pes[6] & 0x04, 0x04);
        // PTS_DTS_flags = 11
        assert_eq!(pes[7] & 0xC0, 0xC0);
        // header_data_length = 10
        assert_eq!(pes[8], 10);
        // PTS 首字节高4位 = 0011
        assert_eq!(pes[9] & 0xF0, 0x30);
        // 末尾是 payload
        assert_eq!(&pes[pes.len() - 2..], &[0xAA, 0xBB]);
    }

    #[test]
    fn audio_pes_writes_packet_length() {
        let pes = build_pes(0xC0, 90000, None, &[0xAA, 0xBB, 0xCC]);
        // 3 bytes optional flags/header length + 5 bytes PTS + payload.
        assert_eq!(u16::from_be_bytes([pes[4], pes[5]]), 11);
    }
}
