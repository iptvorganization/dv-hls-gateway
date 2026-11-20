//! AC-4 ISO-BMFF sample entries / DSI and conversion of one AC-4 sample to
//! one syncframe, without decoding or modifying its encoded payload.
//!
//! ETSI TS 103 190-1 V1.4.1, annexes E and G; TS 103 190-2 V1.3.1,
//! annex E. A conventional MP4 sample is a raw AC-4 frame; MPEG-TS carries
//! the annex G syncframe, which adds a sync word and a size field.

use anyhow::{anyhow, ensure, Context};

use super::boxes::{checked_boxes, BoxView};

const AC40: [u8; 2] = [0xac, 0x40];
const AC41: [u8; 2] = [0xac, 0x41];
// One frame per aligned audio PES. The 16-bit PES_packet_length counts
// the three flag/size bytes, five PTS bytes and this complete syncframe.
const MAX_TS_SYNC_FRAME: usize = u16::MAX as usize - 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ac4Config {
    pub dsi_version: u8,
    pub bitstream_version: u8,
    /// Base sampling frequency represented by fs_index, not the output rate
    /// of a potential decoding/sampling-frequency extension.
    pub sample_rate: u32,
    pub frame_rate_index: u8,
    pub presentations: u16,
    // Proven media compatibility for one supported default presentation.
    master_presentation: Option<(u8, u8)>,
}

impl Ac4Config {
    /// Find the AC-4 configuration in a whole initialization segment or an
    /// already-selected whole trak box. Ambiguous multiple audio tracks must
    /// be selected by the caller instead of taking the first DAC4-like bytes.
    pub fn find_in_init(init_or_trak: &[u8]) -> crate::Result<Option<Self>> {
        let Some(entry) = find_audio_entry(init_or_trak)? else {
            return Ok(None);
        };
        if effective_format(entry)? != *b"ac-4" {
            return Ok(None);
        }
        let children = audio_entry_children(entry)?;
        let dsi = unique_box(children, b"dac4")?
            .context("AC-4 sample entry has no dac4 configuration box")?;
        Self::parse_dsi(dsi.payload).map(Some)
    }

    /// Parse the DSI header and validate length-delimited v1 presentation
    /// records. Presentation data itself stays opaque to this remuxer: the
    /// renderer obtains its configuration from the TOC in each raw frame.
    pub fn parse_dsi(dsi: &[u8]) -> crate::Result<Self> {
        parse_dsi_header(dsi).context("invalid or unsupported AC-4 dac4 configuration")
    }

    /// Check that this frame's AC-4 TOC matches the declared track config,
    /// then preserve an existing syncframe or wrap raw bytes in AC40.
    pub fn wrap_sync_frame(&self, sample: &[u8]) -> crate::Result<Vec<u8>> {
        frame_for_ts(sample, Some(self))
    }

    /// Dolby's four-part HLS codec identifier. Do not infer a default or
    /// compatibility level when the DSI cannot identify it unambiguously.
    pub(crate) fn master_codec_string(&self) -> Option<String> {
        let (presentation_version, md_compat) = self.master_presentation?;
        Some(format!(
            "ac-4.{:02x}.{presentation_version:02x}.{md_compat:02x}",
            self.bitstream_version,
        ))
    }
}

/// Resolve the audio sample entry type. For an enca entry, this reads
/// sinf/frma metadata only; nothing here modifies or handles encryption.
pub fn audio_sample_entry(init_or_trak: &[u8]) -> crate::Result<Option<[u8; 4]>> {
    find_audio_entry(init_or_trak)?
        .map(effective_format)
        .transpose()
}

/// Locate a codec config only inside the selected sample entry. MOV-style
/// audio entries may store their configuration in a direct `wave` child.
pub(super) fn audio_sample_entry_config<'a>(
    init_or_trak: &'a [u8],
    format: &[u8; 4],
    tag: &[u8; 4],
) -> crate::Result<Option<&'a [u8]>> {
    let Some(entry) = find_audio_entry(init_or_trak)? else {
        return Ok(None);
    };
    if &effective_format(entry)? != format {
        return Ok(None);
    }
    let children = audio_entry_children(entry)?;
    let direct = unique_box(children, tag)?;
    let nested = match unique_box(children, b"wave")? {
        Some(wave) => unique_box(wave.payload, tag)?,
        None => None,
    };
    ensure!(
        direct.is_none() || nested.is_none(),
        "ambiguous audio config box"
    );
    Ok(direct.or(nested).map(|entry| entry.payload))
}

fn unique_box<'a>(bytes: &'a [u8], tag: &[u8; 4]) -> crate::Result<Option<BoxView<'a>>> {
    let mut found = None;
    for child in checked_boxes(bytes) {
        let child = child.context("truncated MP4 initialization box")?.view;
        if &child.typ == tag {
            ensure!(
                found.replace(child).is_none(),
                "duplicate {} box in MP4 initialization",
                child.type_str()
            );
        }
    }
    Ok(found)
}

fn find_audio_entry(bytes: &[u8]) -> crate::Result<Option<BoxView<'_>>> {
    // Accept both the complete moov-based init and the complete selected trak.
    let mut found = None;
    for node in checked_boxes(bytes) {
        let node = node.context("truncated MP4 initialization segment")?.view;
        match &node.typ {
            b"moov" => {
                for child in checked_boxes(node.payload) {
                    let child = child.context("truncated MP4 movie box")?.view;
                    if &child.typ == b"trak" {
                        collect_audio_entry(child, &mut found)?;
                    }
                }
            }
            b"trak" => collect_audio_entry(node, &mut found)?,
            _ => {}
        }
    }
    Ok(found)
}

fn collect_audio_entry<'a>(
    trak: BoxView<'a>,
    found: &mut Option<BoxView<'a>>,
) -> crate::Result<()> {
    let Some(mdia) = unique_box(trak.payload, b"mdia")? else {
        return Ok(());
    };
    let Some(handler) = unique_box(mdia.payload, b"hdlr")? else {
        return Ok(());
    };
    let handler_type = handler.payload.get(8..12).context("truncated MP4 hdlr")?;
    if handler_type != b"soun" {
        return Ok(());
    }
    let minf = unique_box(mdia.payload, b"minf")?.context("MP4 audio track is missing minf")?;
    let stbl = unique_box(minf.payload, b"stbl")?.context("MP4 audio track is missing stbl")?;
    let stsd = unique_box(stbl.payload, b"stsd")?.context("MP4 audio track is missing stsd")?;
    let (count, entries) = stsd
        .payload
        .get(4..)
        .and_then(|data| data.split_first_chunk::<4>())
        .context("truncated MP4 stsd header")?;
    ensure!(
        u32::from_be_bytes(*count) == 1,
        "MP4 audio track needs one sample entry"
    );
    let mut entries = checked_boxes(entries);
    let entry = entries
        .next()
        .flatten()
        .context("truncated MP4 audio sample entry")?
        .view;
    ensure!(
        entries.next().is_none(),
        "unexpected additional MP4 audio sample entry"
    );
    ensure!(
        found.replace(entry).is_none(),
        "MP4 init has multiple audio tracks; select one trak first"
    );
    Ok(())
}

fn effective_format(entry: BoxView<'_>) -> crate::Result<[u8; 4]> {
    if &entry.typ != b"enca" {
        return Ok(entry.typ);
    }
    let children = audio_entry_children(entry)?;
    let sinf = unique_box(children, b"sinf")?.context("enca sample entry is missing sinf")?;
    let frma = unique_box(sinf.payload, b"frma")?.context("enca sample entry is missing frma")?;
    frma.payload
        .try_into()
        .context("invalid MP4 frma original format")
}

fn audio_entry_children(entry: BoxView<'_>) -> crate::Result<&[u8]> {
    let version = entry
        .payload
        .get(8..10)
        .context("truncated MP4 audio sample entry")?;
    let offset = match version {
        [0, 0] => 28, // ISO AudioSampleEntry header before child boxes
        [0, 1] => 44, // QuickTime v1 (used by some protected audio wrappers)
        [0, 2] => 64, // QuickTime v2
        _ => return Err(anyhow!("unsupported MP4 audio sample entry version")),
    };
    entry
        .payload
        .get(offset..)
        .context("truncated MP4 audio sample entry header")
}

fn parse_dsi_header(bytes: &[u8]) -> Option<Ac4Config> {
    let mut bits = Bits { bytes, pos: 0 };
    let dsi_version = bits.read(3)? as u8;
    let bitstream_version = bits.read(7)? as u8;
    let fs_index = bits.read(1)? as u8;
    let frame_rate_index = bits.read(4)? as u8;
    let presentations = bits.read(9)? as u16;
    let sample_rate = clock_from_indices(fs_index, frame_rate_index)?;
    let mut master_presentation = None;
    match dsi_version {
        0 if bitstream_version <= 1 => {
            // v0 has no size prefix for its variable-size presentation
            // records. Only the common header is used by the muxer.
            if presentations != 0 && !bits.skip(1) {
                return None;
            }
        }
        1 => {
            if bitstream_version > 1 && bits.read(1)? != 0 {
                if !bits.skip(16) {
                    return None;
                }
                if bits.read(1)? != 0 && !bits.skip(128) {
                    return None;
                }
            }
            // ac4_bitrate_dsi = mode (2), bitrate (32), precision (32).
            if !bits.skip(66) || !bits.skip((8 - bits.pos % 8) % 8) {
                return None;
            }
            for _ in 0..presentations {
                let version = bits.read(8)? as u8;
                let mut len = bits.read(8)? as usize;
                if len == 255 {
                    len += bits.read(16)? as usize;
                }
                let start = bits.pos / 8;
                if !bits.skip(len.checked_mul(8)?) {
                    return None;
                }
                if bitstream_version >= 2 && presentations == 1 && version == 1 && len >= 4 {
                    // ETSI TS 103 190-2 E.10.1: presentation_config_v1(5)
                    // followed by md_compat(3), except for EMDF-only config 6.
                    let first = bytes[start];
                    let level = first & 7;
                    if first >> 3 != 6 && level <= 4 {
                        master_presentation = Some((version, level));
                    }
                }
            }
        }
        _ => return None,
    }
    Some(Ac4Config {
        dsi_version,
        bitstream_version,
        sample_rate,
        frame_rate_index,
        presentations,
        master_presentation,
    })
}

struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn skip(&mut self, count: usize) -> bool {
        let Some(end) = self.pos.checked_add(count) else {
            return false;
        };
        if end.div_ceil(8) > self.bytes.len() {
            return false;
        }
        self.pos = end;
        true
    }

    fn read(&mut self, count: usize) -> Option<u32> {
        let start = self.pos;
        if count > u32::BITS as usize || !self.skip(count) {
            return None;
        }
        Some((start..self.pos).fold(0, |value, bit| {
            (value << 1) | u32::from((self.bytes[bit / 8] >> (7 - bit % 8)) & 1)
        }))
    }
}

fn clock_from_indices(fs_index: u8, frame_rate_index: u8) -> Option<u32> {
    match (fs_index, frame_rate_index) {
        (0, 13) => Some(44_100),
        (1, 0..=13) => Some(48_000),
        _ => None,
    }
}

/// Only read the common AC-4 TOC prefix used for validation (not the
/// presentation or compressed audio data).
fn frame_parameters(raw: &[u8]) -> Option<(u8, u32, u8)> {
    let mut bits = Bits { bytes: raw, pos: 0 };
    let mut version = bits.read(2)?;
    if version == 3 {
        let mut extra = 0u32;
        loop {
            extra = extra.checked_add(bits.read(2)?)?;
            if bits.read(1)? == 0 {
                break;
            }
            extra = extra.checked_add(1)?.checked_mul(4)?;
        }
        version = version.checked_add(extra)?;
    }
    bits.read(10)?; // sequence_counter
    if bits.read(1)? != 0 && bits.read(3)? != 0 {
        bits.read(2)?; // br_code
    }
    let fs_index = bits.read(1)? as u8;
    let frame_rate_index = bits.read(4)? as u8;
    bits.read(1)?; // b_iframe_global
    Some((
        version.try_into().ok()?,
        clock_from_indices(fs_index, frame_rate_index)?,
        frame_rate_index,
    ))
}

/// Frame one sample for MPEG-TS even where only the codec type is known.
/// Prefer `Ac4Config::wrap_sync_frame` when declared DSI is available.
pub fn wrap_sync_frame(sample: &[u8]) -> crate::Result<Vec<u8>> {
    frame_for_ts(sample, None)
}

fn frame_for_ts(sample: &[u8], config: Option<&Ac4Config>) -> crate::Result<Vec<u8>> {
    let framed = sample.starts_with(&AC40) || sample.starts_with(&AC41);
    let raw = if framed {
        raw_from_sync_frame(sample)?
    } else {
        sample
    };
    let sync_frame_len = if framed {
        Some(sample.len())
    } else {
        raw.len().checked_add(4)
    };
    ensure!(
        sync_frame_len.is_some_and(|len| len <= MAX_TS_SYNC_FRAME),
        "AC-4 syncframe is too large for one aligned MPEG-TS audio PES"
    );
    let (bitstream_version, sample_rate, frame_rate_index) =
        frame_parameters(raw).context("invalid or truncated AC-4 frame TOC")?;
    if let Some(config) = config {
        ensure!(
            (bitstream_version, sample_rate, frame_rate_index)
                == (
                    config.bitstream_version,
                    config.sample_rate,
                    config.frame_rate_index
                ),
            "AC-4 frame TOC does not match its dac4 track configuration"
        );
    }

    if framed {
        return Ok(sample.to_vec());
    }
    // Newly built frames use AC40 (no CRC). The PES limit above makes a
    // short, 16-bit Annex G length field sufficient for every generated frame.
    let size = u16::try_from(raw.len())?;
    let mut output = Vec::with_capacity(raw.len() + 4);
    output.extend_from_slice(&AC40);
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(raw);
    Ok(output)
}

fn raw_from_sync_frame(sample: &[u8]) -> crate::Result<&[u8]> {
    // AC40/AC41 cannot also be a valid raw TOC prefix: interpreting either
    // as a TOC sets wait_frames=false, fs_index=0 and an illegal rate < 13.
    let short = u16::from_be_bytes(
        sample
            .get(2..4)
            .context("truncated AC-4 syncframe length")?
            .try_into()?,
    );
    let (header, len) = if short == u16::MAX {
        let size = sample
            .get(4..7)
            .context("truncated extended AC-4 syncframe length")?;
        (
            7usize,
            (usize::from(size[0]) << 16) | (usize::from(size[1]) << 8) | usize::from(size[2]),
        )
    } else {
        (4usize, usize::from(short))
    };
    let end = header
        .checked_add(len)
        .context("AC-4 syncframe size overflow")?;
    let crc_present = sample.starts_with(&AC41);
    let expected = end.checked_add(if crc_present { 2 } else { 0 });
    ensure!(
        len != 0 && expected == Some(sample.len()),
        "AC-4 syncframe length does not match sample"
    );
    ensure!(
        sample.len() <= MAX_TS_SYNC_FRAME,
        "AC-4 syncframe is too large for one aligned MPEG-TS audio PES"
    );
    if crc_present {
        ensure!(crc16(&sample[2..]) == 0, "invalid AC-4 AC41 CRC");
    }
    Ok(&sample[header..end])
}

fn crc16(data: &[u8]) -> u16 {
    // Annex G.4.2: poly 0x8005, initial zero, no reflection or final XOR.
    data.iter().fold(0u16, |mut crc, byte| {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
        crc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Standards-derived synthetic bit prefixes; these are not decoder tests.
    // DSI v1, bitstream v2, 48 kHz, 25 fps, one presentation record.
    const DSI: [u8; 15] = [0x20, 0xa4, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0];
    const RAW: &[u8] = &[0x80, 0x04, 0xa0, 1, 2, 3];

    fn atom(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        [
            (8 + payload.len() as u32).to_be_bytes().as_slice(),
            typ,
            payload,
        ]
        .concat()
    }

    fn trak(handler: &[u8; 4], typ: &[u8; 4], children: &[u8]) -> Vec<u8> {
        let entry = atom(typ, &[&[0; 28], children].concat());
        let stsd = atom(
            b"stsd",
            &[&[0, 0, 0, 0, 0, 0, 0, 1], entry.as_slice()].concat(),
        );
        let hdlr = atom(b"hdlr", &[&[0; 8], handler.as_slice()].concat());
        let minf = atom(b"minf", &atom(b"stbl", &stsd));
        atom(b"trak", &atom(b"mdia", &[hdlr, minf].concat()))
    }

    #[test]
    fn structural_dsi_and_resolved_sample_entry() {
        let children = atom(b"dac4", &DSI);
        let sound = trak(b"soun", b"ac-4", &children);
        let video = trak(b"vide", b"avc1", &children); // must not match copy in another track
        let moov = atom(b"moov", &[video, sound.clone()].concat());
        assert_eq!(audio_sample_entry(&moov).unwrap(), Some(*b"ac-4"));
        let cfg = Ac4Config::find_in_init(&sound).unwrap().unwrap();
        assert_eq!(
            cfg,
            Ac4Config {
                dsi_version: 1,
                bitstream_version: 2,
                sample_rate: 48_000,
                frame_rate_index: 2,
                presentations: 1,
                master_presentation: None,
            }
        );
        assert_eq!(Ac4Config::find_in_init(&moov).unwrap(), Some(cfg));
        let ambiguous = atom(b"moov", &[sound.clone(), sound].concat());
        assert!(Ac4Config::find_in_init(&ambiguous).is_err());

        let protected = [children, atom(b"sinf", &atom(b"frma", b"ac-4"))].concat();
        let sound = trak(b"soun", b"enca", &protected);
        assert_eq!(audio_sample_entry(&sound).unwrap(), Some(*b"ac-4"));
        assert_eq!(Ac4Config::find_in_init(&sound).unwrap(), Some(cfg));
        assert_eq!(
            Ac4Config::find_in_init(&trak(b"soun", b"ec-3", &protected)).unwrap(),
            None
        );
    }

    #[test]
    fn fails_missing_duplicate_or_truncated_dsi() {
        let truncated = atom(b"dac4", &DSI[..DSI.len() - 1]);
        assert!(Ac4Config::find_in_init(&trak(b"soun", b"ac-4", &truncated)).is_err());
        assert!(Ac4Config::find_in_init(&trak(b"soun", b"ac-4", &[])).is_err());
        let children = atom(b"dac4", &DSI);
        let doubled = [children.clone(), children].concat();
        assert!(Ac4Config::find_in_init(&trak(b"soun", b"ac-4", &doubled)).is_err());
    }

    #[test]
    fn master_codec_requires_a_supported_unambiguous_presentation() {
        // Dolby's clear 25 fps stereo reference sample, DSI v1 with one
        // presentation (version 1, stereo compatibility level 0).
        let dsi =
            hex::decode("20a401400000001fffffffe0010ff88000004200000250100000030080").unwrap();
        assert_eq!(
            Ac4Config::parse_dsi(&dsi)
                .unwrap()
                .master_codec_string()
                .as_deref(),
            Some("ac-4.02.01.00")
        );
        assert!(Ac4Config::parse_dsi(&DSI)
            .unwrap()
            .master_codec_string()
            .is_none());
    }

    #[test]
    fn builds_short_sync_frame_preserving_payload_and_dsi_consistency() {
        let cfg = Ac4Config::parse_dsi(&DSI).unwrap();
        let framed = cfg.wrap_sync_frame(RAW).unwrap();
        assert_eq!(framed, [&[0xac, 0x40, 0, 6], RAW].concat());
        assert_eq!(cfg.wrap_sync_frame(&framed).unwrap(), framed);
        let mut mismatch = RAW.to_vec();
        mismatch[2] ^= 0x40;
        assert!(cfg.wrap_sync_frame(&mismatch).is_err());
        assert!(wrap_sync_frame(&[]).is_err());
    }

    #[test]
    fn retains_extended_size_and_ac41_crc_but_rejects_invalid_syncframes() {
        // Known non-reflected CRC-16/0x8005 example (CRC-16/UMTS before output
        // reflection, i.e. the AC-4 CRC): check=0xFEE8.
        assert_eq!(crc16(b"123456789"), 0xfee8);
        let mut framed = [&[0xac, 0x41, 0xff, 0xff, 0, 0, 6], RAW].concat();
        framed.extend_from_slice(&crc16(&framed[2..]).to_be_bytes());
        assert_eq!(wrap_sync_frame(&framed).unwrap(), framed);
        assert!(wrap_sync_frame(&framed[..framed.len() - 1]).is_err());
        framed[7] ^= 1;
        assert!(wrap_sync_frame(&framed)
            .unwrap_err()
            .to_string()
            .contains("CRC"));
    }

    #[test]
    fn enforces_aligned_pes_length() {
        let mut raw = vec![0; MAX_TS_SYNC_FRAME - 4];
        raw[..RAW.len()].copy_from_slice(RAW);
        assert_eq!(wrap_sync_frame(&raw).unwrap().len(), MAX_TS_SYNC_FRAME);
        raw.push(0);
        assert!(wrap_sync_frame(&raw).is_err());
    }
}
