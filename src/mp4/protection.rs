//! Reject protection metadata in the Clear input mode, before parsing samples.

use super::boxes::{iter_boxes, BoxView};
use super::TrackEncryption;

#[derive(Debug, thiserror::Error)]
#[error("Clear mode requires unencrypted input: {track} contains {signal}")]
pub(crate) struct ProtectedInput {
    track: String,
    signal: &'static str,
}

impl ProtectedInput {
    pub(crate) fn new(track: &str, signal: &'static str) -> Self {
        Self {
            track: track.to_owned(),
            signal,
        }
    }
}

pub(crate) fn ensure_clear_init(data: &[u8], track: &str) -> crate::Result<()> {
    if let Some(signal) = init_protection_signal(data, 0) {
        return Err(ProtectedInput::new(track, signal).into());
    }
    if TrackEncryption::find_in_init(data).is_some_and(|tenc| tenc.is_protected) {
        return Err(ProtectedInput::new(track, "a protected tenc box").into());
    }
    Ok(())
}

fn init_protection_signal(data: &[u8], depth: usize) -> Option<&'static str> {
    if depth >= 8 {
        return None;
    }
    iter_boxes(data)
        .into_iter()
        .find_map(|part| match &part.typ {
            b"pssh" => Some("a pssh box"),
            b"stsd" => iter_boxes(part.payload.get(8..)?)
                .iter()
                .any(|entry| matches!(&entry.typ, b"encv" | b"enca" | b"enct" | b"encs" | b"encm"))
                .then_some("a protected sample entry"),
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => {
                init_protection_signal(part.payload, depth + 1)
            }
            _ => None,
        })
}

pub(crate) fn ensure_clear_fragment(data: &[u8], track: &str) -> crate::Result<()> {
    for moof in iter_boxes(data)
        .into_iter()
        .filter(|part| &part.typ == b"moof")
    {
        for traf in iter_boxes(moof.payload)
            .into_iter()
            .filter(|part| &part.typ == b"traf")
        {
            if let Some(signal) = iter_boxes(traf.payload)
                .into_iter()
                .find_map(fragment_signal)
            {
                return Err(ProtectedInput::new(track, signal).into());
            }
        }
    }
    Ok(())
}

fn fragment_signal(part: BoxView<'_>) -> Option<&'static str> {
    match &part.typ {
        b"senc" => Some("a senc box"),
        b"sbgp" | b"sgpd" if part.payload.get(4..8) == Some(b"seig") => Some("a seig sample group"),
        b"saiz" | b"saio"
            if part.payload.get(3).is_some_and(|flags| flags & 1 != 0)
                && matches!(
                    part.payload.get(4..8),
                    Some(b"cenc" | b"cens" | b"cbc1" | b"cbcs")
                ) =>
        {
            Some("CENC auxiliary information")
        }
        _ => None,
    }
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

    #[test]
    fn clear_init_rejects_protected_track_and_sample_entry() {
        let mut tenc = [0; 24];
        assert!(ensure_clear_init(&atom(b"tenc", &tenc), "video init").is_ok());
        tenc[6] = 1;
        let err = ensure_clear_init(&atom(b"tenc", &tenc), "video init").unwrap_err();
        assert!(err.is::<ProtectedInput>());

        let mut stsd = vec![0; 8];
        stsd[7] = 1;
        stsd.extend(atom(b"enca", &[]));
        assert!(ensure_clear_init(&atom(b"stsd", &stsd), "audio init").is_err());
    }

    #[test]
    fn clear_fragment_checks_box_headers_not_sample_payload() {
        let senc = atom(b"senc", &[0; 8]);
        let clear = atom(b"mdat", &senc);
        assert!(ensure_clear_fragment(&clear, "video").is_ok());
        let protected = atom(b"moof", &atom(b"traf", &senc));
        assert!(ensure_clear_fragment(&protected, "video").is_err());
        let seig = atom(b"sgpd", b"\0\0\0\0seig");
        let protected = atom(b"moof", &atom(b"traf", &seig));
        assert!(ensure_clear_fragment(&protected, "audio").is_err());
        assert!(ensure_clear_fragment(b"WEBVTT\n\n", "subtitle").is_ok());
    }
}
