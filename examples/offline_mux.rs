//! 离线验证：用真实加密段 + key，解密并封装成一个 .ts，供 ffprobe 检查 DV RPU 是否保留。
//!
//! 运行：
//!   cargo run --example offline_mux -- \
//!     <seg_dir> <out.ts> <video-kid:key> <audio-kid:key>

use std::path::Path;

use dv_hls_gateway::clock::ClockState;
use dv_hls_gateway::crypto::cenc::{parse_key_hex, Decryptor};
use dv_hls_gateway::hevc::annexb::AccessUnit;
use dv_hls_gateway::mp4::sample::parse_media_segment;
use dv_hls_gateway::mp4::{DoviConfig, ParamSets};
use dv_hls_gateway::ts::muxer::{AudioUnit, TsMuxer};
use dv_hls_gateway::ts::AudioCodec;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(seg_dir) = args.next() else {
        anyhow::bail!("usage: cargo run --example offline_mux -- <seg_dir> <out.ts> <video-kid:key> <audio-kid:key>");
    };
    let Some(out) = args.next() else {
        anyhow::bail!("missing out.ts");
    };
    let Some(vkey_text) = args.next() else {
        anyhow::bail!("missing video kid:key");
    };
    let Some(akey_text) = args.next() else {
        anyhow::bail!("missing audio kid:key");
    };

    let vkey = parse_key_hex(&vkey_text).ok_or_else(|| anyhow::anyhow!("invalid video key"))?;
    let akey = parse_key_hex(&akey_text).ok_or_else(|| anyhow::anyhow!("invalid audio key"))?;

    let d = Path::new(&seg_dir);
    let vinit = std::fs::read(d.join("video/init.mp4"))?;
    let params = ParamSets::find_in_init(&vinit).expect("hvcC");
    let dovi = DoviConfig::find_in_init(&vinit);
    println!(
        "[init] VPS={} SPS={} PPS={}  dovi={:?}",
        params.vps.len(),
        params.sps.len(),
        params.pps.len(),
        dovi
    );

    let vdec = Decryptor::new(vkey);
    let adec = Decryptor::new(akey);

    // 视频 timescale 24000, 音频 48000（已知）
    let v_timescale = 24000u32;
    let a_timescale = 48000u32;

    // 先扫第一段算最小 cts（B 帧可负），用于 composition 平移
    let vseg0 = std::fs::read(d.join("video/seg_0.mp4"))?;
    let p0 = parse_media_segment(&vseg0).expect("parse vseg0");
    let min_cts = p0
        .samples
        .iter()
        .map(|s| s.cts_offset as i64)
        .min()
        .unwrap_or(0);
    let clock = ClockState::new(min_cts, v_timescale, 90_000);
    println!(
        "[clock] min_cts={} cts_shift={} base_offset_90k={}",
        min_cts, clock.cts_shift, clock.base_offset_90k
    );

    let mut muxer = TsMuxer::new(
        &params,
        dv_hls_gateway::ts::VideoCodec::Hevc,
        dovi,
        Some(AudioCodec::Ec3),
        a_timescale,
        clock,
    );
    if let Some(desc) = muxer.dovi_descriptor_bytes() {
        println!("[dovi] descriptor = {}", hex(&desc));
    }

    let mut all = Vec::new();
    let mut acc_v: u64 = 0;
    let mut acc_a: u64 = 0;

    for i in 0..2 {
        // 视频
        let venc = std::fs::read(d.join(format!("video/seg_{i}.mp4")))?;
        let mut vp = parse_media_segment(&venc).expect("parse vseg");
        vdec.decrypt_segment(&mut vp.mdat, &vp.samples);
        let mut vaus = Vec::new();
        for s in &vp.samples {
            let (a, b) = s.data_range;
            let b = b.min(vp.mdat.len());
            let dts = acc_v;
            acc_v += s.duration as u64;
            vaus.push(AccessUnit::from_sample(
                &vp.mdat[a..b],
                dts,
                s.cts_offset as i64,
                dv_hls_gateway::ts::VideoCodec::Hevc,
            ));
        }

        // 音频
        let aenc = std::fs::read(d.join(format!("audio/seg_{i}.mp4")))?;
        let mut ap = parse_media_segment(&aenc).expect("parse aseg");
        adec.decrypt_segment(&mut ap.mdat, &ap.samples);
        let mut aaus = Vec::new();
        for s in &ap.samples {
            let (x, y) = s.data_range;
            let y = y.min(ap.mdat.len());
            let pts = acc_a;
            acc_a += s.duration as u64;
            aaus.push(AudioUnit {
                data: ap.mdat[x..y].to_vec(),
                pts,
            });
        }

        println!(
            "[seg {i}] video AUs={} (first irap={}), audio AUs={}",
            vaus.len(),
            vaus.first().map(|a| a.is_irap).unwrap_or(false),
            aaus.len()
        );
        let ts = muxer.mux_segment(&vaus, &aaus);
        all.extend_from_slice(&ts);
    }

    std::fs::write(&out, &all)?;
    println!(
        "[done] wrote {} bytes ({} TS packets) -> {}",
        all.len(),
        all.len() / 188,
        out
    );
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join("")
}
