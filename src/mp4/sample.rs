//! 媒体段 (moof+mdat) 解析：提取每个样本的字节、时长、cts、IV、subsample 加密映射。
//!
//! 实测：
//! - trun flags=0xb05 → data_offset + first_sample_flags + sample_duration + sample_size + cts
//! - senc 视频 flags=0x2(subsample)，每样本 8B IV + subsample 列表
//! - senc 音频 flags=0x0(整样本)，tenc iv_size=0 → constant IV（来自 tenc）

use super::boxes::{find_box, iter_boxes, CONTAINERS};

/// 一个 subsample 加密映射：clear 字节数 + encrypted 字节数。
#[derive(Debug, Clone, Copy)]
pub struct SubSample {
    pub clear: u16,
    pub encrypted: u32,
}

/// 一个样本（一帧）的全部信息。
#[derive(Debug, Clone)]
pub struct SampleInfo {
    pub data_range: (usize, usize), // 在 mdat payload 内的 [start,end)
    pub duration: u32,
    pub cts_offset: i32,
    /// 每样本 IV（8 或 16 字节）；为空表示用 constant IV。
    pub iv: Vec<u8>,
    /// subsample 列表；为空表示整样本加密。
    pub subsamples: Vec<SubSample>,
}

/// 解析后的整段。
#[derive(Debug, Clone)]
pub struct ParsedSegment {
    pub samples: Vec<SampleInfo>,
    /// mdat payload 字节（密文）。
    pub mdat: Vec<u8>,
    /// tfdt 的 baseMediaDecodeTime（本轨道 timescale），即该段首样本的绝对解码时间。
    /// CMAF 段必带；缺失时为 None（回退到累加逻辑）。用于把视频/音频时间线锚定到
    /// 源的绝对时基，消除独立累加器的取整漂移并保留源固有 A/V offset。
    pub base_media_decode_time: Option<u64>,
}

/// 解析一个 fMP4 媒体段。默认会从 `senc` 字节长度启发式推断 IV 长度。
pub fn parse_media_segment(seg: &[u8]) -> Option<ParsedSegment> {
    parse_media_segment_with_default_iv_size(seg, None)
}

/// 解析一个 fMP4 媒体段，并使用 init `tenc` 的 default_Per_Sample_IV_Size。
/// `cbcs` 常见 `iv_size=0` + constant IV，此时 `senc` 每个 sample 只有 subsample 表。
pub fn parse_media_segment_with_default_iv_size(
    seg: &[u8],
    default_iv_size: Option<u8>,
) -> Option<ParsedSegment> {
    let moof = find_box(seg, b"moof", CONTAINERS)?;
    let traf = find_box(moof, b"traf", CONTAINERS)?;

    // trun
    let trun = find_box(traf, b"trun", CONTAINERS)?;
    let (mut durations, mut sizes, cts, base_data_offset_present, sample_count) = parse_trun(trun)?;

    // tfhd：若 TRUN 不含 per-sample 字段（常见于音频段，count>0 但 dur/size 都在 tfhd），
    // 用 default_sample_duration / default_sample_size 回填。
    // 否则 0 sample → nsamples=0 → 音频帧全丢。
    let (default_sample_duration, default_sample_size) = find_box(traf, b"tfhd", CONTAINERS)
        .map(|h| (parse_tfhd_default_duration(h), parse_tfhd_default_size(h)))
        .unwrap_or((None, None));
    if sizes.is_empty() && sample_count > 0 {
        if let Some(ds) = default_sample_size {
            sizes = vec![ds; sample_count];
        }
    }
    if durations.is_empty() && sample_count > 0 {
        if let Some(dd) = default_sample_duration {
            durations = vec![dd; sample_count];
        }
    }

    // senc（可能不存在，则无加密）
    let senc = find_box(traf, b"senc", CONTAINERS);
    let (ivs, subsamples) = match senc {
        Some(s) => parse_senc(s, default_iv_size)?,
        None => (Vec::new(), Vec::new()),
    };

    // mdat payload
    let mdat = iter_boxes(seg)
        .into_iter()
        .find(|b| &b.typ == b"mdat")
        .map(|b| b.payload.to_vec())?;

    // tfdt（moof>traf>tfdt）：该段首样本绝对解码时间。v0=32bit / v1=64bit。
    let base_media_decode_time = find_box(traf, b"tfdt", CONTAINERS).and_then(parse_tfdt);

    let n = sizes.len();
    let mut samples = Vec::with_capacity(n);
    let mut off = 0usize;
    for i in 0..n {
        let sz = sizes[i] as usize;
        let dur = durations.get(i).copied().unwrap_or(0);
        let cto = cts.get(i).copied().unwrap_or(0);
        let iv = ivs.get(i).cloned().unwrap_or_default();
        let subs = subsamples.get(i).cloned().unwrap_or_default();
        samples.push(SampleInfo {
            data_range: (off, off + sz),
            duration: dur,
            cts_offset: cto,
            iv,
            subsamples: subs,
        });
        off += sz;
    }
    let _ = base_data_offset_present;
    Some(ParsedSegment {
        samples,
        mdat,
        base_media_decode_time,
    })
}

/// 解析 tfdt box payload，返回 baseMediaDecodeTime。version 1 = 64bit，version 0 = 32bit。
fn parse_tfdt(tfdt: &[u8]) -> Option<u64> {
    if tfdt.is_empty() {
        return None;
    }
    let version = tfdt[0];
    if version == 1 {
        if tfdt.len() < 12 {
            return None;
        }
        Some(u64::from_be_bytes([
            tfdt[4], tfdt[5], tfdt[6], tfdt[7], tfdt[8], tfdt[9], tfdt[10], tfdt[11],
        ]))
    } else {
        if tfdt.len() < 8 {
            return None;
        }
        Some(u32::from_be_bytes([tfdt[4], tfdt[5], tfdt[6], tfdt[7]]) as u64)
    }
}

/// 解析 tfhd box，提取 default_sample_duration（若 flags 含 0x000008）。
fn parse_tfhd_default_duration(tfhd: &[u8]) -> Option<u32> {
    parse_tfhd_field(tfhd, 0x000008)
}

/// 解析 tfhd box，提取 default_sample_size（若 flags 含 0x000010）。
fn parse_tfhd_default_size(tfhd: &[u8]) -> Option<u32> {
    parse_tfhd_field(tfhd, 0x000010)
}

fn parse_tfhd_field(tfhd: &[u8], target_flag: u32) -> Option<u32> {
    if tfhd.len() < 8 {
        return None;
    }
    let flags = u32::from_be_bytes([0, tfhd[1], tfhd[2], tfhd[3]]);
    let mut p = 8; // skip version(1)+flags(3)+track_id(4)
    if flags & 0x000001 != 0 {
        p += 8;
    }
    if flags & 0x000002 != 0 {
        p += 4;
    }
    // 按 flag 顺序找字段：0x000008(default_sample_duration) 在 0x000010(default_sample_size) 之前
    if target_flag == 0x000008 && (flags & 0x000008 != 0) {
        if p + 4 <= tfhd.len() {
            return Some(u32::from_be_bytes([
                tfhd[p],
                tfhd[p + 1],
                tfhd[p + 2],
                tfhd[p + 3],
            ]));
        }
    }
    if flags & 0x000008 != 0 {
        p += 4;
    } // skip duration
    if target_flag == 0x000010 && (flags & 0x000010 != 0) {
        if p + 4 <= tfhd.len() {
            return Some(u32::from_be_bytes([
                tfhd[p],
                tfhd[p + 1],
                tfhd[p + 2],
                tfhd[p + 3],
            ]));
        }
    }
    None
}

/// 解析 trun，返回 (durations, sizes, cts_offsets, base_data_offset_present, sample_count)。
fn parse_trun(trun: &[u8]) -> Option<(Vec<u32>, Vec<u32>, Vec<i32>, bool, usize)> {
    if trun.len() < 8 {
        return None;
    }
    let version = trun[0];
    let flags = u32::from_be_bytes([0, trun[1], trun[2], trun[3]]);
    let count = u32::from_be_bytes([trun[4], trun[5], trun[6], trun[7]]) as usize;
    let mut p = 8;
    let data_offset_present = flags & 0x000001 != 0;
    let first_sample_flags_present = flags & 0x000004 != 0;
    let dur_present = flags & 0x000100 != 0;
    let size_present = flags & 0x000200 != 0;
    let flags_present = flags & 0x000400 != 0;
    let cts_present = flags & 0x000800 != 0;

    if data_offset_present {
        p += 4;
    }
    if first_sample_flags_present {
        p += 4;
    }

    let mut durations = Vec::with_capacity(count);
    let mut sizes = Vec::with_capacity(count);
    let mut cts = Vec::with_capacity(count);
    for _ in 0..count {
        if dur_present {
            durations.push(u32::from_be_bytes([
                trun[p],
                trun[p + 1],
                trun[p + 2],
                trun[p + 3],
            ]));
            p += 4;
        }
        if size_present {
            sizes.push(u32::from_be_bytes([
                trun[p],
                trun[p + 1],
                trun[p + 2],
                trun[p + 3],
            ]));
            p += 4;
        }
        if flags_present {
            p += 4;
        }
        if cts_present {
            let raw = u32::from_be_bytes([trun[p], trun[p + 1], trun[p + 2], trun[p + 3]]);
            // version 1: signed; version 0: unsigned
            let v = if version == 0 { raw as i32 } else { raw as i32 };
            cts.push(v);
            p += 4;
        }
    }
    Some((durations, sizes, cts, data_offset_present, count))
}

/// 解析 senc，返回 (每样本 IV, 每样本 subsample 列表)。
/// 视频：8B per-sample IV + subsamples；音频：可能 IV 长度 0（constant IV，外部提供）。
fn parse_senc(
    senc: &[u8],
    default_iv_size: Option<u8>,
) -> Option<(Vec<Vec<u8>>, Vec<Vec<SubSample>>)> {
    if senc.len() < 8 {
        return None;
    }
    let flags = u32::from_be_bytes([0, senc[1], senc[2], senc[3]]);
    let use_subsamples = flags & 0x2 != 0;
    let mut p = 4;
    let mut iv_size = default_iv_size.map(|v| v as usize);
    if flags & 0x1 != 0 {
        if p + 20 > senc.len() {
            return None;
        }
        iv_size = Some(senc[p + 3] as usize);
        p += 20; // AlgorithmID(24) + IV_size(8) + KID(16)
    }
    if p + 4 > senc.len() {
        return None;
    }
    let count = u32::from_be_bytes([senc[p], senc[p + 1], senc[p + 2], senc[p + 3]]) as usize;
    p += 4;

    // IV 长度：优先使用 init/tenc 或 senc override；缺失时再从剩余字节推断。
    let iv_size = iv_size
        .or_else(|| infer_iv_size(&senc[p..], count, use_subsamples))
        .unwrap_or(8);

    let mut ivs = Vec::with_capacity(count);
    let mut subs = Vec::with_capacity(count);
    for _ in 0..count {
        if p + iv_size > senc.len() {
            break;
        }
        ivs.push(senc[p..p + iv_size].to_vec());
        p += iv_size;
        if use_subsamples {
            if p + 2 > senc.len() {
                break;
            }
            let nsub = u16::from_be_bytes([senc[p], senc[p + 1]]) as usize;
            p += 2;
            let mut list = Vec::with_capacity(nsub);
            for _ in 0..nsub {
                if p + 6 > senc.len() {
                    break;
                }
                let clear = u16::from_be_bytes([senc[p], senc[p + 1]]);
                let enc = u32::from_be_bytes([senc[p + 2], senc[p + 3], senc[p + 4], senc[p + 5]]);
                p += 6;
                list.push(SubSample {
                    clear,
                    encrypted: enc,
                });
            }
            subs.push(list);
        } else {
            subs.push(Vec::new());
        }
    }
    Some((ivs, subs))
}

/// 启发式推断 senc 的 per-sample IV 长度（8 或 16）。
fn infer_iv_size(sample_data: &[u8], count: usize, use_subsamples: bool) -> Option<usize> {
    let avail = sample_data.len();
    if !use_subsamples {
        // 整样本：avail == count * iv_size
        if count > 0 && avail == count * 8 {
            return Some(8);
        }
        if count > 0 && avail == count * 16 {
            return Some(16);
        }
        if count > 0 && avail == 0 {
            return Some(0); // constant IV
        }
    }
    // 有 subsample 时，先验证 IV=8 能否完整解析
    for &iv in &[8usize, 16usize] {
        if validate_iv(sample_data, count, iv) {
            return Some(iv);
        }
    }
    None
}

fn validate_iv(sample_data: &[u8], count: usize, iv_size: usize) -> bool {
    let mut p = 0;
    for _ in 0..count {
        if p + iv_size + 2 > sample_data.len() {
            return false;
        }
        p += iv_size;
        let nsub = u16::from_be_bytes([sample_data[p], sample_data[p + 1]]) as usize;
        p += 2;
        p += nsub * 6;
        if p > sample_data.len() {
            return false;
        }
    }
    p == sample_data.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_bytes(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn tfdt_v1_64bit() {
        // version=1, flags=0, baseMediaDecodeTime=4518565519 (实测视频段值)
        let mut pl = vec![1, 0, 0, 0];
        pl.extend_from_slice(&4518565519u64.to_be_bytes());
        assert_eq!(parse_tfdt(&pl), Some(4518565519));
    }

    #[test]
    fn tfdt_v0_32bit() {
        let mut pl = vec![0, 0, 0, 0];
        pl.extend_from_slice(&1234567u32.to_be_bytes());
        assert_eq!(parse_tfdt(&pl), Some(1234567));
    }

    #[test]
    fn tfdt_truncated_is_none() {
        assert_eq!(parse_tfdt(&[]), None);
        assert_eq!(parse_tfdt(&[1, 0, 0, 0, 0]), None); // v1 但不足 12 字节
        assert_eq!(parse_tfdt(&[0, 0, 0, 0, 0]), None); // v0 但不足 8 字节
    }

    /// 构造一个最小 moof(traf(tfdt+trun)) + mdat 段，验证 base_media_decode_time 被填充
    /// 且样本 duration 正常解析（确保新增 tfdt 解析不破坏既有 trun/mdat 逻辑）。
    #[test]
    fn parse_segment_extracts_tfdt_and_samples() {
        // tfdt v1
        let mut tfdt_pl = vec![1u8, 0, 0, 0];
        tfdt_pl.extend_from_slice(&90000u64.to_be_bytes());
        let tfdt = box_bytes(b"tfdt", &tfdt_pl);

        // trun: flags=0x000301 (data_offset + duration + size)，2 个样本
        let mut trun_pl = vec![0u8, 0x00, 0x03, 0x01];
        trun_pl.extend_from_slice(&2u32.to_be_bytes()); // sample_count
        trun_pl.extend_from_slice(&0u32.to_be_bytes()); // data_offset
        trun_pl.extend_from_slice(&1024u32.to_be_bytes()); // s0 dur
        trun_pl.extend_from_slice(&3u32.to_be_bytes()); // s0 size
        trun_pl.extend_from_slice(&1024u32.to_be_bytes()); // s1 dur
        trun_pl.extend_from_slice(&4u32.to_be_bytes()); // s1 size
        let trun = box_bytes(b"trun", &trun_pl);

        let mut traf_pl = Vec::new();
        traf_pl.extend_from_slice(&tfdt);
        traf_pl.extend_from_slice(&trun);
        let traf = box_bytes(b"traf", &traf_pl);
        let moof = box_bytes(b"moof", &traf);

        let mdat = box_bytes(b"mdat", &[0xAA; 7]); // 3 + 4 字节

        let mut seg = Vec::new();
        seg.extend_from_slice(&moof);
        seg.extend_from_slice(&mdat);

        let parsed = parse_media_segment(&seg).expect("parse");
        assert_eq!(parsed.base_media_decode_time, Some(90000));
        assert_eq!(parsed.samples.len(), 2);
        assert_eq!(parsed.samples[0].duration, 1024);
        assert_eq!(parsed.samples[0].data_range, (0, 3));
        assert_eq!(parsed.samples[1].data_range, (3, 7));
    }

    #[test]
    fn parse_senc_with_constant_iv_and_subsamples() {
        let mut senc = vec![0u8, 0, 0, 2]; // flags=subsample encryption
        senc.extend_from_slice(&2u32.to_be_bytes());
        senc.extend_from_slice(&1u16.to_be_bytes());
        senc.extend_from_slice(&5u16.to_be_bytes());
        senc.extend_from_slice(&32u32.to_be_bytes());
        senc.extend_from_slice(&0u16.to_be_bytes());

        let (ivs, subs) = parse_senc(&senc, Some(0)).unwrap();
        assert_eq!(ivs.len(), 2);
        assert!(ivs.iter().all(|iv| iv.is_empty()));
        assert_eq!(subs[0].len(), 1);
        assert_eq!(subs[0][0].clear, 5);
        assert_eq!(subs[0][0].encrypted, 32);
        assert!(subs[1].is_empty());
    }

    /// tfdt 缺失时 base_media_decode_time 为 None（触发 pipeline 回退到累加逻辑）。
    #[test]
    fn parse_segment_without_tfdt_is_none() {
        let mut trun_pl = vec![0u8, 0x00, 0x03, 0x01];
        trun_pl.extend_from_slice(&1u32.to_be_bytes());
        trun_pl.extend_from_slice(&0u32.to_be_bytes());
        trun_pl.extend_from_slice(&512u32.to_be_bytes());
        trun_pl.extend_from_slice(&2u32.to_be_bytes());
        let trun = box_bytes(b"trun", &trun_pl);
        let traf = box_bytes(b"traf", &trun);
        let moof = box_bytes(b"moof", &traf);
        let mdat = box_bytes(b"mdat", &[1, 2]);
        let mut seg = Vec::new();
        seg.extend_from_slice(&moof);
        seg.extend_from_slice(&mdat);
        let parsed = parse_media_segment(&seg).expect("parse");
        assert_eq!(parsed.base_media_decode_time, None);
        assert_eq!(parsed.samples.len(), 1);
    }
}
