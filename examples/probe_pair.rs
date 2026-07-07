//! 临时：验证 video/audio 段号是否同一套编号 + idx配对 vs number配对差异。
//! cargo run --release --example probe_pair -- <mpd_url> <video_rep_id> <audio_rep_id>
use dv_hls_gateway::mpd::{parse_mpd, TrackKind};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args().nth(1).unwrap();
    let vid = std::env::args().nth(2).unwrap();
    let aid = std::env::args().nth(3).unwrap();
    let client = reqwest::Client::new();
    let text = client
        .get(&url)
        .header("user-agent", "Mozilla/5.0")
        .send()
        .await?
        .text()
        .await?;
    let mpd = parse_mpd(&text, &url)?;
    let v = mpd
        .representations
        .iter()
        .find(|r| r.id == vid && r.kind == TrackKind::Video)
        .unwrap();
    let a = mpd
        .representations
        .iter()
        .find(|r| r.id == aid && r.kind == TrackKind::Audio)
        .unwrap();
    let vn: Vec<u64> = v.segments.iter().map(|s| s.number).collect();
    let an: Vec<u64> = a.segments.iter().map(|s| s.number).collect();
    println!(
        "VIDEO: {}段 [{}..{}]",
        vn.len(),
        vn.first().unwrap(),
        vn.last().unwrap()
    );
    println!(
        "AUDIO: {}段 [{}..{}]",
        an.len(),
        an.first().unwrap(),
        an.last().unwrap()
    );
    let aset: std::collections::HashSet<u64> = an.iter().copied().collect();
    let matched = vn.iter().filter(|n| aset.contains(n)).count();
    println!(
        "视频段号能在音频中按 number 找到的比例: {}/{}",
        matched,
        vn.len()
    );
    // idx 配对错位检查
    let mut mis = 0;
    for i in 0..vn.len().min(an.len()) {
        if vn[i] != an[i] {
            mis += 1;
        }
    }
    println!(
        "idx 配对错位数(同下标段号不等): {}/{}",
        mis,
        vn.len().min(an.len())
    );
    Ok(())
}
