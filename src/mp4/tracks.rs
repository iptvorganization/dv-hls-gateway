//! Track selection and fragment defaults from an fMP4 initialization segment.

use std::collections::HashMap;

use super::boxes::{checked_boxes, find_box, CONTAINERS};
use super::mdhd::parse_mdhd_timescale;
use super::sample::{next_u32, SampleDefaults};

#[derive(Debug, Clone, Copy)]
pub struct InitTrack<'a> {
    pub track_id: u32,
    /// ISO-BMFF handler type (e.g. vide, soun, subt, text).
    pub handler: [u8; 4],
    pub timescale: u32,
    /// First sample entry type. Protected entries remain encv/enca, not frma.
    pub codec: Option<[u8; 4]>,
    /// Entire trak box, for existing scoped codec/track metadata readers.
    pub trak: &'a [u8],
    pub defaults: SampleDefaults,
}

/// Return tracks in initialization order, or None for a malformed or incomplete init.
pub fn tracks_from_init(init: &[u8]) -> Option<Vec<InitTrack<'_>>> {
    let mut moov = None;
    for b in checked_boxes(init) {
        let b = b?;
        if &b.view.typ == b"moov" && moov.replace(b.view.payload).is_some() {
            return None;
        }
    }
    let moov = moov?;
    let mut tracks = Vec::new();
    let mut defaults = HashMap::new();

    for b in checked_boxes(moov) {
        let b = b?;
        match &b.view.typ {
            b"trak" => tracks.push(parse_track(b.whole, b.view.payload)?),
            b"mvex" => {
                for entry in checked_boxes(b.view.payload) {
                    let entry = entry?.view;
                    if &entry.typ != b"trex" {
                        continue;
                    }
                    let mut p = 4;
                    let track_id = next_u32(entry.payload, &mut p)?;
                    let default_description = next_u32(entry.payload, &mut p)?;
                    let duration = next_u32(entry.payload, &mut p)?;
                    let size = next_u32(entry.payload, &mut p)?;
                    next_u32(entry.payload, &mut p)?; // default sample flags
                    if default_description != 1 {
                        return None; // switching sample descriptions requires reinitializing codec data
                    }
                    if defaults
                        .insert(
                            track_id,
                            SampleDefaults {
                                duration: (duration > 0).then_some(duration),
                                size: (size > 0).then_some(size),
                            },
                        )
                        .is_some()
                    {
                        return None;
                    }
                }
            }
            _ => {}
        }
    }

    let mut seen = std::collections::HashSet::new();
    for track in &mut tracks {
        if !seen.insert(track.track_id) {
            return None;
        }
        if let Some(defaults) = defaults.get(&track.track_id) {
            track.defaults = *defaults;
        }
    }
    (!tracks.is_empty()).then_some(tracks)
}

fn parse_track<'a>(whole: &'a [u8], payload: &'a [u8]) -> Option<InitTrack<'a>> {
    let mut track_id = None;
    let mut mdia = None;
    for child in checked_boxes(payload) {
        let child = child?.view;
        match &child.typ {
            b"tkhd" if track_id.is_none() => {
                let mut pos = match *child.payload.first()? {
                    0 => 12,
                    1 => 20,
                    _ => return None,
                };
                let id = next_u32(child.payload, &mut pos)?;
                if id == 0 {
                    return None;
                }
                track_id = Some(id);
            }
            b"mdia" if mdia.is_none() => mdia = Some(child.payload),
            b"tkhd" | b"mdia" => return None,
            _ => {}
        }
    }

    let mdia = mdia?;
    let mut timescale = None;
    let mut handler = None;
    for child in checked_boxes(mdia) {
        let child = child?.view;
        match &child.typ {
            b"mdhd" if timescale.is_none() => {
                timescale = Some(parse_mdhd_timescale(child.payload)?);
            }
            b"hdlr" if handler.is_none() => {
                handler = Some(child.payload.get(8..12)?.try_into().ok()?);
            }
            b"mdhd" | b"hdlr" => return None,
            _ => {}
        }
    }
    let timescale = timescale.filter(|n| *n != 0)?;
    let sample_entry = if let Some(stsd) = find_box(mdia, b"stsd", CONTAINERS) {
        let declared = next_u32(stsd, &mut 4)?;
        let mut entries = checked_boxes(stsd.get(8..)?);
        if declared > 1 {
            return None;
        }
        let entry = match (declared, entries.next()) {
            (0, None) => None,
            (1, Some(Some(entry))) => Some(entry.view.typ),
            _ => return None,
        };
        if entries.next().is_some() {
            return None;
        }
        entry
    } else {
        None
    };
    Some(InitTrack {
        track_id: track_id?,
        handler: handler?,
        timescale,
        codec: sample_entry,
        trak: whole,
        defaults: SampleDefaults::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        [
            (8 + payload.len() as u32).to_be_bytes().as_slice(),
            typ,
            payload,
        ]
        .concat()
    }

    fn track(id: u32, kind: &[u8; 4], scale: u32, codec: &[u8; 4]) -> Vec<u8> {
        let tkhd = atom(b"tkhd", &[&[0; 12], id.to_be_bytes().as_slice()].concat());
        let mdhd = atom(
            b"mdhd",
            &[&[0; 12], scale.to_be_bytes().as_slice()].concat(),
        );
        let hdlr = atom(b"hdlr", &[&[0; 8], kind.as_slice()].concat());
        let stsd = atom(
            b"stsd",
            &[&[0, 0, 0, 0, 0, 0, 0, 1], atom(codec, &[]).as_slice()].concat(),
        );
        let minf = atom(b"minf", &atom(b"stbl", &stsd));
        let mdia = atom(b"mdia", &[mdhd, hdlr, minf].concat());
        atom(b"trak", &[tkhd, mdia].concat())
    }

    #[test]
    fn identifies_tracks_and_scopes_defaults_in_a_mixed_init() {
        let mut trex = vec![0; 4];
        for field in [7u32, 1, 1024, 32, 0] {
            trex.extend_from_slice(&field.to_be_bytes());
        }
        let moov = atom(
            b"moov",
            &[
                track(3, b"vide", 12_800, b"avc1"),
                track(7, b"soun", 48_000, b"mp4a"),
                atom(b"mvex", &atom(b"trex", &trex)),
            ]
            .concat(),
        );
        let tracks = tracks_from_init(&moov).expect("valid track metadata");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track_id, 3);
        assert_eq!(tracks[0].timescale, 12_800);
        assert_eq!(tracks[0].codec, Some(*b"avc1"));
        assert_eq!(tracks[0].defaults, SampleDefaults::default());
        assert_eq!(tracks[1].track_id, 7);
        assert_eq!(&tracks[1].handler, b"soun");
        assert_eq!(tracks[1].timescale, 48_000);
        assert_eq!(tracks[1].codec, Some(*b"mp4a"));
        assert_eq!(tracks[1].defaults.duration, Some(1024));
        assert_eq!(tracks[1].defaults.size, Some(32));
        assert_eq!(&tracks[1].trak[4..8], b"trak");
    }
}
