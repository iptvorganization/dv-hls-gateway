//! 可选集成测试：用真实加密段 + key 解密，与 mp4decrypt 的黄金输出逐字节比对。
//! 未设置环境变量时自动跳过，避免普通 CI 依赖本机样本。

use std::path::Path;

use dv_hls_gateway::crypto::cenc::{parse_key_hex, Decryptor};
use dv_hls_gateway::mp4::sample::parse_media_segment;

/// 提取 mp4decrypt 输出文件里第一个 moof 段的 mdat（明文）做参照。
/// 我们的输入是单段 video/seg_0.mp4，对应明文样本应与 video_dec.mp4 中前 N 个样本一致。
#[test]
fn cenc_decrypt_matches_mp4decrypt() {
    let Some(seg_dir) = std::env::var("DVHLS_GOLDEN_SEG_DIR").ok() else {
        eprintln!("skip: DVHLS_GOLDEN_SEG_DIR not set");
        return;
    };
    let Some(vkey) = std::env::var("DVHLS_GOLDEN_VIDEO_KEY").ok() else {
        eprintln!("skip: DVHLS_GOLDEN_VIDEO_KEY not set");
        return;
    };
    let d = Path::new(&seg_dir);
    if !d.join("video/seg_0.mp4").exists() {
        eprintln!("skip: 测试数据不存在");
        return;
    }

    let key = parse_key_hex(&vkey).unwrap();
    let dec = Decryptor::new(key);

    // 我方：解密 seg_0
    let enc = std::fs::read(d.join("video/seg_0.mp4")).unwrap();
    let mut parsed = parse_media_segment(&enc).expect("parse");
    let n_samples = parsed.samples.len();
    assert!(n_samples > 0);
    dec.decrypt_segment(&mut parsed.mdat, &parsed.samples);

    // 黄金：mp4decrypt 把 init+seg_0+seg_1 合并解密成 video_dec.mp4。
    // 它的第一个 mdat 即 seg_0 的明文样本数据。
    let golden_full = std::fs::read(d.join("video_dec.mp4")).unwrap();
    let golden_mdat = first_mdat(&golden_full).expect("golden mdat");

    // 比对前若干样本（按我方 data_range），逐字节。
    // 注意：黄金 mdat 是连续样本拼接，与我方 mdat 同布局（同 seg 同顺序）。
    let compare_len = parsed.mdat.len().min(golden_mdat.len());
    assert!(
        compare_len > 100_000,
        "compare_len too small: {compare_len}"
    );

    let mismatch = parsed.mdat[..compare_len]
        .iter()
        .zip(&golden_mdat[..compare_len])
        .position(|(a, b)| a != b);
    assert_eq!(
        mismatch, None,
        "解密结果与 mp4decrypt 黄金输出在偏移 {:?} 不一致",
        mismatch
    );

    // 额外：确认第一帧含 DV RPU (NAL type 62)
    let (a, b) = parsed.samples[0].data_range;
    let sample0 = &parsed.mdat[a..b.min(parsed.mdat.len())];
    assert!(has_nal_type(sample0, 62), "第一帧应含 DV RPU (NAL 62)");
}

/// 取文件里第一个 mdat box 的 payload。
fn first_mdat(data: &[u8]) -> Option<&[u8]> {
    let mut p = 0;
    while p + 8 <= data.len() {
        let size = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize;
        let typ = &data[p + 4..p + 8];
        if typ == b"mdat" {
            let end = if size == 0 {
                data.len()
            } else {
                (p + size).min(data.len())
            };
            return Some(&data[p + 8..end]);
        }
        if size < 8 {
            break;
        }
        p += size;
    }
    None
}

/// 扫描 length-prefixed 样本数据里是否含某 NAL 类型。
fn has_nal_type(sample: &[u8], want: u8) -> bool {
    let mut p = 0;
    while p + 4 <= sample.len() {
        let len =
            u32::from_be_bytes([sample[p], sample[p + 1], sample[p + 2], sample[p + 3]]) as usize;
        p += 4;
        if len == 0 || p + len > sample.len() {
            break;
        }
        let t = (sample[p] >> 1) & 0x3F;
        if t == want {
            return true;
        }
        p += len;
    }
    false
}

#[test]
fn audio_decrypt_ec3_syncword() {
    let Some(seg_dir) = std::env::var("DVHLS_GOLDEN_SEG_DIR").ok() else {
        eprintln!("skip: DVHLS_GOLDEN_SEG_DIR not set");
        return;
    };
    let Some(akey) = std::env::var("DVHLS_GOLDEN_AUDIO_KEY").ok() else {
        eprintln!("skip: DVHLS_GOLDEN_AUDIO_KEY not set");
        return;
    };
    let d = Path::new(&seg_dir);
    if !d.join("audio/seg_0.mp4").exists() {
        return;
    }
    let key = parse_key_hex(&akey).unwrap();
    let dec = Decryptor::new(key);
    let enc = std::fs::read(d.join("audio/seg_0.mp4")).unwrap();
    let mut parsed = parse_media_segment(&enc).expect("parse");
    eprintln!(
        "audio samples={}, first iv len={}, subsamples={}",
        parsed.samples.len(),
        parsed.samples[0].iv.len(),
        parsed.samples[0].subsamples.len()
    );
    dec.decrypt_segment(&mut parsed.mdat, &parsed.samples);
    // 第一个样本应以 EC-3 syncword 0b77 开头
    let (a, _) = parsed.samples[0].data_range;
    eprintln!("audio sample0 first bytes: {:02x?}", &parsed.mdat[a..a + 4]);
    assert_eq!(
        &parsed.mdat[a..a + 2],
        &[0x0b, 0x77],
        "EC-3 syncword mismatch"
    );
}
