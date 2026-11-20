//! 媒体段 (moof+mdat) 解析：提取每个样本的字节、时长、cts、IV、subsample 加密映射。
//!
//! 实测：
//! - trun flags=0xb05 → data_offset + first_sample_flags + sample_duration + sample_size + cts
//! - senc 视频 flags=0x2(subsample)，每样本 8B IV + subsample 列表
//! - senc 音频 flags=0x0(整样本)，tenc iv_size=0 → constant IV（来自 tenc）

use super::boxes::checked_boxes;
use std::ops::Range;

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
    /// 选中轨道的样本数据，按解码顺序紧密排列；data_range 指向此缓冲区。
    pub mdat: Vec<u8>,
    /// tfdt 的 baseMediaDecodeTime（本轨道 timescale），即该段首样本的绝对解码时间。
    /// CMAF 段必带；缺失时为 None（回退到累加逻辑）。用于把视频/音频时间线锚定到
    /// 源的绝对时基，消除独立累加器的取整漂移并保留源固有 A/V offset。
    pub base_media_decode_time: Option<u64>,
    /// 各样本相对首样本的 DTS，保留同一 HTTP 段内各 moof 之间的间隙。
    pub decode_time_offsets: Vec<u64>,
}

/// 解析一个 fMP4 媒体段。默认会从 `senc` 字节长度启发式推断 IV 长度。
pub fn parse_media_segment(seg: &[u8]) -> Option<ParsedSegment> {
    parse_media_segment_with_default_iv_size(seg, None)
}

/// 解析单轨 fMP4 媒体段。多轨输入必须显式选择 track ID。
pub fn parse_media_segment_with_default_iv_size(
    seg: &[u8],
    default_iv_size: Option<u8>,
) -> Option<ParsedSegment> {
    parse_segment(seg, None, default_iv_size, SampleDefaults::default())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SampleDefaults {
    pub duration: Option<u32>,
    pub size: Option<u32>,
}

/// 解析指定轨道的完整 fMP4 媒体段；defaults 来自该轨道 init 中的 trex。
pub fn parse_media_segment_for_track(
    seg: &[u8],
    track_id: u32,
    default_iv_size: Option<u8>,
    defaults: SampleDefaults,
) -> Option<ParsedSegment> {
    parse_segment(seg, Some(track_id), default_iv_size, defaults)
}

/// 上限用于限制每段样本元数据的开销；普通短段远低于这个数量。
const MAX_SAMPLES_PER_SEGMENT: usize = 262_144;

fn parse_segment(
    seg: &[u8],
    requested_track: Option<u32>,
    default_iv_size: Option<u8>,
    trex: SampleDefaults,
) -> Option<ParsedSegment> {
    let mut mdats = Vec::new();
    let mut payload_bytes = 0usize;
    for b in checked_boxes(seg) {
        let b = b?;
        if &b.view.typ == b"mdat" {
            let end = b.start + b.whole.len();
            payload_bytes += b.view.payload.len();
            mdats.push(end - b.view.payload.len()..end);
        }
    }
    if mdats.is_empty() {
        return None;
    }

    let mut track_id = requested_track;
    let mut found = false;
    let mut samples = Vec::new();
    let mut mdat = Vec::new();
    let mut decode_time_offsets = Vec::new();
    let mut base_media_decode_time = None;
    let mut next_decode_offset = 0u64;
    let mut sample_budget = MAX_SAMPLES_PER_SEGMENT;

    for moof in checked_boxes(seg) {
        let moof = moof?;
        if &moof.view.typ != b"moof" {
            continue;
        }
        let mut implicit_base = Some(moof.start);
        for traf in checked_boxes(moof.view.payload) {
            let traf = traf?;
            if &traf.view.typ != b"traf" {
                continue;
            }
            let traf = TrackFragment::parse(traf.view.payload, &mut sample_budget)?;
            let wanted_track = *track_id.get_or_insert(traf.header.track_id);
            let selected = traf.header.track_id == wanted_track;
            if requested_track.is_none() && !selected {
                return None;
            }
            found |= selected;

            // ISO-BMFF: a later traf without an explicit/default-moof base
            // inherits the data end of its predecessor, not the mdat start.
            let base = traf.header.base_data_offset.or_else(|| {
                if traf.header.default_base_is_moof {
                    Some(moof.start)
                } else {
                    implicit_base
                }
            });
            let trex = if selected {
                trex
            } else {
                SampleDefaults::default()
            };
            let defaults = SampleDefaults {
                duration: traf.header.defaults.duration.or(trex.duration),
                size: traf.header.defaults.size.or(trex.size),
            };
            let mut run_end = base;
            let mut decode_offset = next_decode_offset;
            let mut encryption = None;
            if selected && traf.sample_count != 0 {
                if samples.is_empty() {
                    base_media_decode_time = traf.decode_time;
                } else if let (Some(base), Some(time)) = (base_media_decode_time, traf.decode_time)
                {
                    decode_offset = time.checked_sub(base)?;
                    if decode_offset < next_decode_offset {
                        return None;
                    }
                }
                if let Some(senc) = traf.senc {
                    let (ivs, subsamples) =
                        parse_senc(senc, default_iv_size, Some(traf.sample_count))?;
                    encryption = Some(ivs.into_iter().zip(subsamples));
                }
            }

            for b in checked_boxes(traf.data) {
                let b = b?;
                if &b.view.typ != b"trun" {
                    continue;
                }
                let run = TrackRun::parse(b.view.payload)?;
                let start = if let Some(relative) = run.data_offset {
                    base.and_then(|base| base.checked_add_signed(relative as isize))
                } else {
                    run_end
                };
                if run.count == 0 {
                    run_end = start;
                    continue;
                }
                // Non-selected tracks may need their own trex default size;
                // when it is unknown, only following tracks with an explicit
                // base remain addressable.
                let Some(start) = start else {
                    if selected {
                        return None;
                    }
                    run_end = None;
                    continue;
                };
                let Some(size) = run.total_size(defaults.size) else {
                    if selected {
                        return None;
                    }
                    run_end = None;
                    continue;
                };
                let end = start.checked_add(size)?;
                if !inside_mdat(&mdats, start, end) {
                    return None;
                }
                run_end = Some(end);
                if !selected {
                    continue;
                }

                if mdat.len().checked_add(size)? > payload_bytes {
                    return None;
                }
                let run_data = seg.get(start..end)?;
                let mut offset = 0usize;
                for i in 0..run.count {
                    let fields = run.sample(i, defaults)?;
                    let sample_end = offset.checked_add(fields.size as usize)?;
                    let payload = run_data.get(offset..sample_end)?;
                    let (iv, subsamples) = if let Some(iterator) = &mut encryption {
                        iterator.next()?
                    } else {
                        (Vec::new(), Vec::new())
                    };
                    let payload_start = mdat.len();
                    mdat.extend_from_slice(payload);
                    samples.push(SampleInfo {
                        data_range: (payload_start, mdat.len()),
                        duration: fields.duration,
                        cts_offset: fields.cts_offset,
                        iv,
                        subsamples,
                    });
                    decode_time_offsets.push(decode_offset);
                    decode_offset = decode_offset.checked_add(u64::from(fields.duration))?;
                    offset = sample_end;
                }
            }
            if selected {
                if let Some(base) = base_media_decode_time {
                    base.checked_add(decode_offset)?;
                }
                next_decode_offset = decode_offset;
            }
            implicit_base = run_end;
        }
    }
    found.then_some(ParsedSegment {
        samples,
        mdat,
        base_media_decode_time,
        decode_time_offsets,
    })
}

fn inside_mdat(mdats: &[Range<usize>], start: usize, end: usize) -> bool {
    mdats
        .partition_point(|mdat| mdat.start <= start)
        .checked_sub(1)
        .is_some_and(|i| end <= mdats[i].end)
}

struct TrackFragment<'a> {
    header: TrackFragmentHeader,
    decode_time: Option<u64>,
    senc: Option<&'a [u8]>,
    sample_count: usize,
    data: &'a [u8],
}

impl<'a> TrackFragment<'a> {
    fn parse(data: &'a [u8], sample_budget: &mut usize) -> Option<Self> {
        let mut header = None;
        let mut decode_time = None;
        let mut senc = None;
        let mut sample_count = 0usize;

        for b in checked_boxes(data) {
            let b = b?.view;
            match &b.typ {
                b"tfhd" if header.is_none() => {
                    header = Some(TrackFragmentHeader::parse(b.payload)?)
                }
                b"tfdt" if decode_time.is_none() => decode_time = Some(parse_tfdt(b.payload)?),
                b"senc" if senc.is_none() => senc = Some(b.payload),
                b"tfhd" | b"tfdt" | b"senc" => return None,
                b"trun" => {
                    let n = TrackRun::parse(b.payload)?.count;
                    *sample_budget = sample_budget.checked_sub(n)?;
                    sample_count += n;
                }
                _ => {}
            }
        }

        Some(Self {
            header: header?,
            decode_time,
            senc,
            sample_count,
            data,
        })
    }
}

struct TrackFragmentHeader {
    track_id: u32,
    base_data_offset: Option<usize>,
    default_base_is_moof: bool,
    defaults: SampleDefaults,
}

impl TrackFragmentHeader {
    fn parse(data: &[u8]) -> Option<Self> {
        const BASE_DATA_OFFSET_PRESENT: u32 = 0x000001;
        const DESCRIPTION_INDEX_PRESENT: u32 = 0x000002;
        const DEFAULT_DURATION_PRESENT: u32 = 0x000008;
        const DEFAULT_SIZE_PRESENT: u32 = 0x000010;
        const DEFAULT_SAMPLE_FLAGS_PRESENT: u32 = 0x000020;
        const DEFAULT_BASE_IS_MOOF: u32 = 0x020000;

        let (version, flags) = full_box(data)?;
        if version != 0 {
            return None;
        }
        let mut p = 4;
        let track_id = next_u32(data, &mut p)?;
        if track_id == 0 {
            return None;
        }
        let base_data_offset = if flags & BASE_DATA_OFFSET_PRESENT != 0 {
            let offset = u64::from_be_bytes(data.get(p..p + 8)?.try_into().ok()?);
            p += 8;
            Some(usize::try_from(offset).ok()?)
        } else {
            None
        };
        if flags & DESCRIPTION_INDEX_PRESENT != 0 && next_u32(data, &mut p)? != 1 {
            return None; // sample descriptions other than the first are not initialized by callers
        }
        let duration = if flags & DEFAULT_DURATION_PRESENT != 0 {
            Some(next_u32(data, &mut p)?)
        } else {
            None
        };
        let size = if flags & DEFAULT_SIZE_PRESENT != 0 {
            Some(next_u32(data, &mut p)?)
        } else {
            None
        };
        if flags & DEFAULT_SAMPLE_FLAGS_PRESENT != 0 {
            next_u32(data, &mut p)?;
        }
        Some(Self {
            track_id,
            base_data_offset,
            default_base_is_moof: flags & DEFAULT_BASE_IS_MOOF != 0,
            defaults: SampleDefaults { duration, size },
        })
    }
}

struct TrackRun<'a> {
    version: u8,
    data_offset: Option<i32>,
    count: usize,
    entries: &'a [u8],
    stride: usize,
    duration_present: bool,
    size_present: bool,
    sample_flags_present: bool,
    cts_present: bool,
}

impl<'a> TrackRun<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        const DATA_OFFSET_PRESENT: u32 = 0x000001;
        const FIRST_SAMPLE_FLAGS_PRESENT: u32 = 0x000004;

        let (version, flags) = full_box(data)?;
        if version > 1 {
            return None;
        }
        let mut p = 4;
        let count = next_u32(data, &mut p)? as usize;
        let data_offset = if flags & DATA_OFFSET_PRESENT != 0 {
            Some(next_u32(data, &mut p)? as i32)
        } else {
            None
        };
        if flags & FIRST_SAMPLE_FLAGS_PRESENT != 0 {
            next_u32(data, &mut p)?;
        }
        let duration_present = flags & 0x000100 != 0;
        let size_present = flags & 0x000200 != 0;
        let sample_flags_present = flags & 0x000400 != 0;
        let cts_present = flags & 0x000800 != 0;
        let stride = 4 * [
            duration_present,
            size_present,
            sample_flags_present,
            cts_present,
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        let bytes = count.checked_mul(stride)?;
        let entries = data.get(p..p.checked_add(bytes)?)?;
        if p + bytes != data.len() {
            return None;
        }

        Some(Self {
            version,
            data_offset,
            count,
            entries,
            stride,
            duration_present,
            size_present,
            sample_flags_present,
            cts_present,
        })
    }

    fn total_size(&self, default_size: Option<u32>) -> Option<usize> {
        if !self.size_present {
            return self.count.checked_mul(default_size? as usize);
        }
        let size_offset = if self.duration_present { 4 } else { 0 };
        self.entries
            .chunks_exact(self.stride)
            .try_fold(0usize, |total, entry| {
                let size = u32::from_be_bytes(entry[size_offset..size_offset + 4].try_into().ok()?);
                total.checked_add(size as usize)
            })
    }

    fn sample(&self, index: usize, defaults: SampleDefaults) -> Option<RunSample> {
        let mut p = index.checked_mul(self.stride)?;
        let duration = if self.duration_present {
            next_u32(self.entries, &mut p)?
        } else {
            defaults.duration?
        };
        let size = if self.size_present {
            next_u32(self.entries, &mut p)?
        } else {
            defaults.size?
        };
        if self.sample_flags_present {
            next_u32(self.entries, &mut p)?;
        }
        let cts_offset = if self.cts_present {
            let raw = next_u32(self.entries, &mut p)?;
            if self.version == 0 {
                i32::try_from(raw).ok()? // v0 is unsigned; cannot represent larger offsets
            } else {
                raw as i32
            }
        } else {
            0
        };
        Some(RunSample {
            duration,
            size,
            cts_offset,
        })
    }
}

struct RunSample {
    duration: u32,
    size: u32,
    cts_offset: i32,
}

fn full_box(data: &[u8]) -> Option<(u8, u32)> {
    let [version, hi, mid, lo] = data.get(..4)?.try_into().ok()?;
    Some((version, u32::from_be_bytes([0, hi, mid, lo])))
}

pub(super) fn next_u32(data: &[u8], pos: &mut usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    let n = u32::from_be_bytes(data.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(n)
}

fn parse_tfdt(data: &[u8]) -> Option<u64> {
    match *data.first()? {
        0 => Some(u64::from(next_u32(data, &mut 4)?)),
        1 => Some(u64::from_be_bytes(data.get(4..12)?.try_into().ok()?)),
        _ => None,
    }
}

/// 解析 senc，返回 (每样本 IV, 每样本 subsample 列表)。
/// 视频：8B per-sample IV + subsamples；音频：可能 IV 长度 0（constant IV，外部提供）。

fn parse_senc(
    senc: &[u8],
    default_iv_size: Option<u8>,
    expected_count: Option<usize>,
) -> Option<(Vec<Vec<u8>>, Vec<Vec<SubSample>>)> {
    let (_, flags) = full_box(senc)?;
    let use_subsamples = flags & 0x2 != 0;
    let mut p = 4;
    let mut iv_size = default_iv_size.map(usize::from);
    if flags & 0x1 != 0 {
        // AlgorithmID(24) + IV_size(8) + KID(16)
        iv_size = Some(usize::from(*senc.get(p + 3)?));
        p += 20;
    }
    let count = next_u32(senc, &mut p)? as usize;
    if count > MAX_SAMPLES_PER_SEGMENT || expected_count.is_some_and(|n| count != n) {
        return None;
    }

    let iv_size = iv_size
        .or_else(|| infer_iv_size(senc.get(p..)?, count, use_subsamples))
        .unwrap_or(8);
    if !matches!(iv_size, 0 | 8 | 16) {
        return None;
    }
    let lower_bound = count.checked_mul(iv_size + if use_subsamples { 2 } else { 0 })?;
    if senc.len().checked_sub(p)? < lower_bound {
        return None;
    }

    let mut ivs = Vec::with_capacity(count);
    let mut subs = Vec::with_capacity(count);
    for _ in 0..count {
        ivs.push(senc.get(p..p + iv_size)?.to_vec());
        p += iv_size;
        if use_subsamples {
            let nsub = u16::from_be_bytes(senc.get(p..p + 2)?.try_into().ok()?) as usize;
            p += 2;
            let end = p.checked_add(nsub * 6)?;
            let fields = senc.get(p..end)?;
            let mut list = Vec::with_capacity(nsub);
            for field in fields.chunks_exact(6) {
                list.push(SubSample {
                    clear: u16::from_be_bytes(field[..2].try_into().ok()?),
                    encrypted: u32::from_be_bytes(field[2..].try_into().ok()?),
                });
            }
            subs.push(list);
            p = end;
        } else {
            subs.push(Vec::new());
        }
    }
    (p == senc.len()).then_some((ivs, subs))
}

/// 启发式推断 senc 的 per-sample IV 长度（8 或 16）。
fn infer_iv_size(data: &[u8], count: usize, use_subsamples: bool) -> Option<usize> {
    if !use_subsamples {
        if data.is_empty() {
            return Some(0); // constant IV
        }
        return [8, 16]
            .into_iter()
            .find(|iv| count.checked_mul(*iv) == Some(data.len()));
    }

    [8, 16].into_iter().find(|iv| validate_iv(data, count, *iv))
}

fn validate_iv(data: &[u8], count: usize, iv_size: usize) -> bool {
    let mut p = 0;
    for _ in 0..count {
        p += iv_size;
        let Some(nsub) = data.get(p..p + 2).and_then(|b| b.try_into().ok()) else {
            return false;
        };
        p += 2 + usize::from(u16::from_be_bytes(nsub)) * 6;
        if p > data.len() {
            return false;
        }
    }
    p == data.len()
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

    fn tfhd(track_id: u32) -> Vec<u8> {
        let mut header = vec![0, 2, 0, 0]; // default-base-is-moof
        header.extend_from_slice(&track_id.to_be_bytes());
        box_bytes(b"tfhd", &header)
    }

    fn tfdt(time: u64) -> Vec<u8> {
        let mut header = vec![1, 0, 0, 0];
        header.extend_from_slice(&time.to_be_bytes());
        box_bytes(b"tfdt", &header)
    }

    fn trun(data_offset: Option<i32>, fields: &[(u32, u32)]) -> Vec<u8> {
        let mut payload = vec![0, 0, 0x03, u8::from(data_offset.is_some())];
        payload.extend_from_slice(&(fields.len() as u32).to_be_bytes());
        if let Some(offset) = data_offset {
            payload.extend_from_slice(&offset.to_be_bytes());
        }
        for (duration, size) in fields {
            payload.extend_from_slice(&duration.to_be_bytes());
            payload.extend_from_slice(&size.to_be_bytes());
        }
        box_bytes(b"trun", &payload)
    }

    fn chunk(time: Option<u64>) -> Vec<u8> {
        let make_moof = |offset| {
            let mut payload = tfhd(1);
            if let Some(time) = time {
                payload.extend(tfdt(time));
            }
            payload.extend(trun(Some(offset), &[(1024, 3)]));
            payload.extend(trun(None, &[(1024, 4)]));
            box_bytes(b"moof", &box_bytes(b"traf", &payload))
        };

        let mut seg = make_moof(make_moof(0).len() as i32 + 8);
        seg.extend(box_bytes(b"mdat", &[0xAA; 7]));
        seg
    }

    #[test]
    fn parses_multiple_moofs_and_truns_without_discarding_decode_gaps() {
        let mut seg = chunk(Some(90_000));
        seg.extend(chunk(Some(93_000)));
        let parsed = parse_media_segment(&seg).expect("all fragments");
        assert_eq!(parsed.base_media_decode_time, Some(90_000));
        assert_eq!(parsed.decode_time_offsets, [0, 1024, 3000, 4024]);
        assert_eq!(parsed.samples.len(), 4);
        assert_eq!(parsed.samples[0].duration, 1024);
        assert_eq!(parsed.samples[0].data_range, (0, 3));
        assert_eq!(parsed.samples[1].data_range, (3, 7));
        assert_eq!(parsed.samples[2].data_range, (7, 10));
        assert_eq!(parsed.samples[3].data_range, (10, 14));
        assert_eq!(parsed.mdat, [0xAA; 14]);
    }

    #[test]
    fn explicit_track_ids_use_offsets_not_mdat_start_or_traf_order() {
        let build_moof = |mdat_start: i32| {
            let mut first = tfhd(3);
            first.extend(trun(Some(mdat_start + 3), &[(48, 2)]));
            let mut second = tfhd(7);
            second.extend(trun(Some(mdat_start), &[(90, 3)]));
            let trafs = [box_bytes(b"traf", &first), box_bytes(b"traf", &second)].concat();
            box_bytes(b"moof", &trafs)
        };
        let mut seg = build_moof(build_moof(0).len() as i32 + 8);
        seg.extend(box_bytes(b"mdat", &[2, 4, 6, 7, 9]));
        assert!(
            parse_media_segment(&seg).is_none(),
            "mixed needs an explicit ID"
        );
        let first = parse_media_segment_for_track(&seg, 3, None, SampleDefaults::default())
            .expect("first track");
        let second = parse_media_segment_for_track(&seg, 7, None, SampleDefaults::default())
            .expect("second track");
        assert_eq!(first.mdat, [7, 9]);
        assert_eq!(second.mdat, [2, 4, 6]);
        assert!(parse_media_segment_for_track(&seg, 8, None, SampleDefaults::default()).is_none());
    }

    #[test]
    fn signed_offset_and_track_defaults_work_without_sample_table_fields() {
        // An explicit base at the end of an earlier mdat plus a negative
        // offset locates the final two bytes, not the start of the mdat.
        let mut seg = box_bytes(b"mdat", &[1, 8, 9]);
        let mut header = vec![0, 0, 0, 1]; // base_data_offset_present
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(&(seg.len() as u64).to_be_bytes());
        let mut traf = box_bytes(b"tfhd", &header);
        let mut run = vec![0, 0, 0, 1]; // data_offset_present
        run.extend_from_slice(&2u32.to_be_bytes());
        run.extend_from_slice(&(-2i32).to_be_bytes());
        traf.extend(box_bytes(b"trun", &run));
        seg.extend(box_bytes(b"moof", &box_bytes(b"traf", &traf)));
        let defaults = SampleDefaults {
            duration: Some(90),
            size: Some(1),
        };
        let parsed = parse_media_segment_for_track(&seg, 1, None, defaults).expect("track");
        assert_eq!(parsed.mdat, [8, 9]);
        assert_eq!(parsed.decode_time_offsets, [0, 90]);
    }

    #[test]
    fn returns_none_on_incomplete_run_and_media_payload() {
        let mut incomplete = trun(Some(0), &[(5, 4)]);
        // A declared sample duration is present, but its size was truncated.
        incomplete.truncate(incomplete.len() - 4);
        let shorter_len = (incomplete.len() as u32).to_be_bytes();
        incomplete[..4].copy_from_slice(&shorter_len);
        let mut traf = tfhd(1);
        traf.extend(incomplete);
        let mut incorrect = box_bytes(b"moof", &box_bytes(b"traf", &traf));
        incorrect.extend(box_bytes(b"mdat", &[0; 4]));
        assert!(parse_media_segment(&incorrect).is_none());

        let mut seg = chunk(None);
        seg.pop();
        assert!(parse_media_segment(&seg).is_none());
    }

    #[test]
    fn parse_senc_with_constant_iv_and_subsamples() {
        let mut senc = vec![0u8, 0, 0, 2]; // flags=subsample encryption
        senc.extend_from_slice(&2u32.to_be_bytes());
        senc.extend_from_slice(&1u16.to_be_bytes());
        senc.extend_from_slice(&5u16.to_be_bytes());
        senc.extend_from_slice(&32u32.to_be_bytes());
        senc.extend_from_slice(&0u16.to_be_bytes());

        let (ivs, subs) = parse_senc(&senc, Some(0), Some(2)).unwrap();
        assert_eq!(ivs.len(), 2);
        assert!(ivs.iter().all(|iv| iv.is_empty()));
        assert_eq!(subs[0].len(), 1);
        assert_eq!(subs[0][0].clear, 5);
        assert_eq!(subs[0][0].encrypted, 32);
        assert!(subs[1].is_empty());
    }

    #[test]
    fn tfdt_absent_leaves_base_unknown_but_offsets_contiguous() {
        let seg = chunk(None);
        let parsed = parse_media_segment(&seg).expect("parse");
        assert_eq!(parsed.base_media_decode_time, None);
        assert_eq!(parsed.decode_time_offsets, [0, 1024]);
    }
}
