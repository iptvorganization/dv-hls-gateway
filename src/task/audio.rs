//! Shared audio sample-entry and framing rules for the fMP4 inputs.

use crate::mp4::aac::AacConfig;
use crate::mp4::ac4::{audio_sample_entry, Ac4Config};
use crate::ts::AudioCodec;

#[derive(Debug, thiserror::Error)]
#[error("AC-4 with nonzero CTS offset ({0}) is unsupported")]
pub(super) struct UnsupportedAc4CompositionOffset(pub(super) i32);

#[derive(Debug, Clone, Copy)]
pub(super) enum Fmp4AudioConfig {
    Aac(AacConfig),
    Ac3,
    Ec3,
    Ac4(Ac4Config),
}

impl Fmp4AudioConfig {
    /// The selected sample entry determines the codec, not the list of codecs
    /// advertised by a master (which may describe other renditions too).
    pub(super) fn from_init(track: &[u8]) -> crate::Result<Self> {
        match audio_sample_entry(track)?.as_ref() {
            Some(b"ac-3") => Ok(Self::Ac3),
            Some(b"ec-3") => Ok(Self::Ec3),
            Some(b"ac-4") => Ac4Config::find_in_init(track)?
                .map(Self::Ac4)
                .ok_or_else(|| anyhow::anyhow!("AC-4 init has no usable dac4 configuration")),
            Some(b"mp4a") => {
                let config = AacConfig::find_in_init(track).ok_or_else(|| {
                    anyhow::anyhow!("mp4a has no supported AAC esds configuration")
                })?;
                anyhow::ensure!(
                    matches!(config.object_type, 1..=5 | 29) && config.sample_rate() != 0,
                    "MP4 audio object type {} cannot be carried as AAC ADTS",
                    config.object_type
                );
                Ok(Self::Aac(config))
            }
            Some(fourcc) => anyhow::bail!(
                "unsupported fMP4 audio sample entry {:?}",
                String::from_utf8_lossy(fourcc)
            ),
            None => anyhow::bail!("fMP4 audio init has no identified sample entry"),
        }
    }

    pub(super) fn codec(self) -> AudioCodec {
        match self {
            Self::Aac(_) => AudioCodec::AacAdts,
            Self::Ac3 => AudioCodec::Ac3,
            Self::Ec3 => AudioCodec::Ec3,
            Self::Ac4(_) => AudioCodec::Ac4,
        }
    }

    pub(super) fn master_codec_string(self) -> Option<String> {
        match self {
            Self::Aac(config) if config.object_type == 2 => Some("mp4a.40.2".into()),
            Self::Aac(_) => None,
            Self::Ac3 => Some("ac-3".into()),
            Self::Ec3 => Some("ec-3".into()),
            Self::Ac4(config) => config.master_codec_string(),
        }
    }

    pub(super) fn pack_sample(self, sample: &[u8], cts_offset: i32) -> crate::Result<Vec<u8>> {
        match self {
            Self::Aac(config) => {
                anyhow::ensure!(
                    !sample.is_empty() && sample.len() <= 0x1fff - 7,
                    "AAC access unit does not fit an ADTS frame"
                );
                Ok(config.wrap_adts(sample))
            }
            Self::Ac3 | Self::Ec3 => Ok(sample.to_vec()),
            Self::Ac4(config) => {
                // Audio timing in both pipelines (aggregation, gap handling
                // and TS PTS) uses the decode timeline. Reject rather than
                // publish a frame whose actual presentation time differs.
                if cts_offset != 0 {
                    return Err(UnsupportedAc4CompositionOffset(cts_offset).into());
                }
                config.wrap_sync_frame(sample)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [(8 + body.len() as u32).to_be_bytes().as_slice(), kind, body].concat()
    }

    fn audio_track(kind: &[u8; 4], config_box: &[u8]) -> Vec<u8> {
        let mut entry = [0; 28];
        entry[7] = 1; // data_reference_index
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(atom(kind, &[entry.as_slice(), config_box].concat()));
        let hdlr = atom(b"hdlr", b"\0\0\0\0\0\0\0\0soun");
        let stbl = atom(b"stbl", &atom(b"stsd", &stsd));
        atom(
            b"trak",
            &atom(b"mdia", &[hdlr, atom(b"minf", &stbl)].concat()),
        )
    }

    #[test]
    fn sample_entry_is_authoritative_and_unknown_does_not_become_aac() {
        for (fourcc, expected) in [(b"ac-3", AudioCodec::Ac3), (b"ec-3", AudioCodec::Ec3)] {
            let track = audio_track(fourcc, &[]);
            let config = Fmp4AudioConfig::from_init(&track).unwrap();
            assert_eq!(config.codec(), expected);
            assert_eq!(config.pack_sample(b"audio", 0).unwrap(), b"audio");
        }

        let unsupported = audio_track(b"Opus", &[]);
        assert!(Fmp4AudioConfig::from_init(&unsupported)
            .unwrap_err()
            .to_string()
            .contains("unsupported"));
        let broken_ac4 = audio_track(b"ac-4", &[]);
        assert!(Fmp4AudioConfig::from_init(&broken_ac4)
            .unwrap_err()
            .to_string()
            .to_ascii_lowercase()
            .contains("dac4"));
    }

    #[test]
    fn ac4_does_not_silently_drop_a_composition_offset() {
        let dsi = [0x20, 0xa4, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0];
        let track = audio_track(b"ac-4", &atom(b"dac4", &dsi));
        let config = Fmp4AudioConfig::from_init(&track).unwrap();
        assert!(config.pack_sample(&[0x80, 0x04, 0xa0], 0).is_ok());
        for offset in [-1920, 1920] {
            assert!(config
                .pack_sample(&[0x80, 0x04, 0xa0], offset)
                .unwrap_err()
                .to_string()
                .contains("CTS"));
        }
    }
}
