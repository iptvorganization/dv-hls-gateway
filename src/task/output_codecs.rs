//! HLS master CODECS describes the complete output, not the alternatives in
//! the origin master. Omit it when any emitted codec identifier is unknown.

use super::audio::Fmp4AudioConfig;
use crate::mp4::ParamSets;

pub(super) fn verified_fmp4_codecs(
    video_entry: Option<[u8; 4]>,
    params: &ParamSets,
    has_dovi: bool,
    audio: Option<Fmp4AudioConfig>,
) -> Option<String> {
    // Only advertise AVC when the selected avc1 entry's SPS establishes
    // the exact profile/constraint/level bytes. avc3 permits in-band
    // changes that can't be verified from the init alone.
    if video_entry.as_ref() != Some(b"avc1") || has_dovi {
        return None;
    }
    let signature = |sps: &[u8]| -> Option<[u8; 3]> {
        if *sps.first()? & 0x9f != 7 || *sps.get(1)? == 0 || *sps.get(3)? == 0 {
            return None;
        }
        sps.get(1..4)?.try_into().ok()
    };
    let header = signature(params.sps.first()?)?;
    if !params.sps.iter().all(|sps| signature(sps) == Some(header)) {
        return None;
    }
    let mut codecs = format!("avc1.{}", hex::encode(header));
    if let Some(audio) = audio {
        codecs.push(',');
        codecs.push_str(&audio.master_codec_string()?);
    }
    Some(codecs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_codec_from_copied_sps_and_selected_audio() {
        let mut params = ParamSets {
            sps: vec![vec![0x67, 0x42, 0xc0, 0x0a]],
            ..ParamSets::default()
        };
        assert_eq!(
            verified_fmp4_codecs(Some(*b"avc1"), &params, false, Some(Fmp4AudioConfig::Ac3))
                .as_deref(),
            Some("avc1.42c00a,ac-3")
        );
        for entry in [*b"hvc1", *b"avc3", *b"encv"] {
            assert!(verified_fmp4_codecs(Some(entry), &params, false, None).is_none());
        }
        params.sps.push(vec![0x67, 0x42, 0xc0, 0x1f]);
        assert!(verified_fmp4_codecs(Some(*b"avc1"), &params, false, None).is_none());
    }
}
