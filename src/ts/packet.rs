//! TS packet 层：把 PES / section 切成 188 字节 TS 包，处理 adaptation field、PCR、
//! continuity counter、payload_unit_start_indicator、stuffing。

use std::collections::HashMap;

use super::{SYNC_BYTE, TS_PACKET_SIZE};

/// 每个 PID 独立的 continuity counter。
#[derive(Default)]
pub struct ContinuityCounters {
    map: HashMap<u16, u8>,
}

impl ContinuityCounters {
    pub fn new() -> Self {
        Self::default()
    }
    /// 取下一个 cc（有 payload 时调用），4-bit 自增。
    fn next(&mut self, pid: u16) -> u8 {
        let e = self.map.entry(pid).or_insert(0);
        let v = *e;
        *e = (*e + 1) & 0x0F;
        v
    }
}

/// 把 PSI section（PAT/PMT）打包成 TS 包（单包即可，section < 184）。
pub fn pack_section(pid: u16, section: &[u8], cc: &mut ContinuityCounters) -> Vec<u8> {
    let mut pkt = vec![0xFFu8; TS_PACKET_SIZE];
    pkt[0] = SYNC_BYTE;
    // PUSI=1
    pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = (pid & 0xFF) as u8;
    // AFC=01 (仅 payload), cc
    pkt[3] = 0x10 | cc.next(pid);
    // pointer_field=0 + section
    let mut p = 4;
    pkt[p] = 0x00;
    p += 1;
    let n = section.len().min(TS_PACKET_SIZE - p);
    pkt[p..p + n].copy_from_slice(&section[..n]);
    // 其余已是 0xFF stuffing
    pkt
}

/// 把一个 PES packet 打包成连续若干 TS 包。
/// - `pcr90`: 若 Some，在第一个包写 PCR（放 video PID 的 adaptation field）。
/// - `random_access`: 第一个包 adaptation field 的 random_access_indicator（IDR 帧置 true）。
pub fn pack_pes(
    pid: u16,
    pes: &[u8],
    pcr90: Option<u64>,
    random_access: bool,
    cc: &mut ContinuityCounters,
    out: &mut Vec<u8>,
) {
    let mut offset = 0usize;
    let mut first = true;

    while offset < pes.len() {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        let pusi = if first { 0x40 } else { 0x00 };
        pkt[1] = pusi | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;

        let remaining = pes.len() - offset;

        // 是否需要 adaptation field：第一个包(可能带PCR/RAI) 或 末尾不满需 stuffing
        let want_af_first = first && (pcr90.is_some() || random_access);
        let payload_capacity_no_af = TS_PACKET_SIZE - 4;

        if want_af_first {
            // 构造 adaptation field（PCR + flags）
            let mut af = Vec::new();
            // flags 字节
            let mut flags = 0u8;
            if random_access {
                flags |= 0x40;
            }
            let pcr_flag = pcr90.is_some();
            if pcr_flag {
                flags |= 0x10;
            }
            af.push(flags);
            if let Some(pcr) = pcr90 {
                let base = pcr & 0x1_FFFF_FFFF;
                let ext: u16 = 0;
                af.push((base >> 25) as u8);
                af.push((base >> 17) as u8);
                af.push((base >> 9) as u8);
                af.push((base >> 1) as u8);
                af.push((((base & 1) as u8) << 7) | 0x7E | ((ext >> 8) as u8 & 0x01));
                af.push((ext & 0xFF) as u8);
            }
            // 现在决定 payload 能放多少
            // 包结构: [4 header][1 af_len][af bytes][payload]
            let header_and_aflen = 4 + 1;
            let af_content_len = af.len();
            let max_payload = TS_PACKET_SIZE - header_and_aflen - af_content_len;
            let take = remaining.min(max_payload);
            // 若 payload 不足以填满，需要用 stuffing 撑满 af
            let stuffing = max_payload - take;
            let af_len = af_content_len + stuffing;

            pkt[3] = 0x30 | cc.next(pid); // AFC=11 (af+payload)
            pkt[4] = af_len as u8;
            let mut w = 5;
            pkt[w..w + af_content_len].copy_from_slice(&af);
            w += af_content_len;
            for _ in 0..stuffing {
                pkt[w] = 0xFF;
                w += 1;
            }
            pkt[w..w + take].copy_from_slice(&pes[offset..offset + take]);
            offset += take;
        } else if remaining >= payload_capacity_no_af {
            // 满包，仅 payload
            pkt[3] = 0x10 | cc.next(pid); // AFC=01
            let take = payload_capacity_no_af;
            pkt[4..4 + take].copy_from_slice(&pes[offset..offset + take]);
            offset += take;
        } else {
            // 末尾不满，用 adaptation field stuffing 撑满
            // 包结构: [4 header][1 af_len][stuffing][payload]
            let take = remaining;
            // payload + 1(af_len) + stuffing = 184
            let af_len = (TS_PACKET_SIZE - 4) - 1 - take; // stuffing 字节数（af 内容全是 stuffing）
            pkt[3] = 0x30 | cc.next(pid); // AFC=11
            pkt[4] = af_len as u8;
            let mut w = 5;
            // adaptation_field: 若 af_len>=1，第一字节是 flags(=0)，其余 0xFF stuffing
            if af_len >= 1 {
                pkt[w] = 0x00; // flags 全 0
                w += 1;
                for _ in 0..(af_len - 1) {
                    pkt[w] = 0xFF;
                    w += 1;
                }
            }
            pkt[w..w + take].copy_from_slice(&pes[offset..offset + take]);
            offset += take;
        }

        out.extend_from_slice(&pkt);
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_packet_is_188() {
        let mut cc = ContinuityCounters::new();
        let pkt = pack_section(0x0000, &[0x00, 0x01, 0x02], &mut cc);
        assert_eq!(pkt.len(), 188);
        assert_eq!(pkt[0], 0x47);
        assert_eq!(pkt[1] & 0x40, 0x40); // PUSI
    }

    #[test]
    fn pes_packs_to_188_multiples_with_pcr() {
        let mut cc = ContinuityCounters::new();
        let pes = vec![0xAAu8; 400]; // 跨多个包
        let mut out = Vec::new();
        pack_pes(0x0101, &pes, Some(90000), true, &mut cc, &mut out);
        assert_eq!(out.len() % 188, 0);
        // 第一个包应有 AF (AFC=11)
        assert_eq!(out[3] & 0x30, 0x30);
        assert_eq!(out[0], 0x47);
        // 第一个包 PUSI
        assert_eq!(out[1] & 0x40, 0x40);
    }

    #[test]
    fn pes_tail_stuffing_exact_188() {
        let mut cc = ContinuityCounters::new();
        // 选一个会产生不满尾包的长度
        let pes = vec![0xBBu8; 200];
        let mut out = Vec::new();
        pack_pes(0x0101, &pes, None, false, &mut cc, &mut out);
        assert_eq!(out.len() % 188, 0);
    }
}
