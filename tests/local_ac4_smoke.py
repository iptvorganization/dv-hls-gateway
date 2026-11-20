#!/usr/bin/env python3
"""Byte-level AC-4 DASH/HLS→TS regression using loopback HTTP only.

Run from a checkout after building:
    python3 tests/local_ac4_smoke.py --binary target/release/dv-hls-gateway

By default the AC-4 payload of each sample is synthetic (not playback
material). To check real audio bytes, use --reference-mp4 with a local clear
25 fps / 48 kHz fragmented Dolby reference MP4. One suitable source is the
25 fps H.264 stereo fragmented file in Dolby's public AC-4 test kit:
https://ott.dolby.com/OnDelKits/AC-4/Dolby_AC-4_Online_Delivery_Kit_1.5/help_files/topics/kit_wrapper_MP4_multiplexed_streams.html
The script never downloads media, and does not claim device playback.
Add --ffprobe-ac4 to require that a compatible ffprobe identifies each
output's audio stream as AC-4 (no decoder is needed).
"""

import argparse
from pathlib import Path
import struct
import sys
import tempfile
import xml.etree.ElementTree as ET

# Reuse the local HTTP/process harness without leaving a cache in the checkout.
sys.dont_write_bytecode = True
from local_gateway_smoke import (
    Gateway, REPO, Upstream, check, ffmpeg, playlist_times, playlist_urls, probe,
)

AUDIO_TRACK = 7
AUDIO_ID = "ac4-audio"
SAMPLES_PER_SEGMENT = 50
SAMPLE_DURATION = 1920
TIMESCALE = 48000
PMT_DESCRIPTORS = bytes.fromhex("050441432d347f021500")


def atom(kind, payload=b""):
    return struct.pack(">I4s", len(payload) + 8, kind) + payload


def full_atom(kind, payload=b"", *, version=0, flags=0):
    return atom(kind, bytes([version]) + flags.to_bytes(3, "big") + payload)


def atoms(data):
    offset = 0
    while offset < len(data):
        check(offset + 8 <= len(data), "truncated input box")
        length, kind = struct.unpack_from(">I4s", data, offset)
        header = 8
        if length == 1:
            check(offset + 16 <= len(data), "truncated input large box")
            length = struct.unpack_from(">Q", data, offset + 8)[0]
            header = 16
        elif length == 0:
            length = len(data) - offset
        check(header <= length <= len(data) - offset, "invalid input box size")
        yield kind, data[offset + header : offset + length]
        offset += length


def find_box(data, *path):
    for kind in path:
        choices = [body for tag, body in atoms(data) if tag == kind]
        check(len(choices) == 1, f"expected unique {kind!r} box")
        data = choices[0]
    return data


def synthetic_fixture():
    # A 48 kHz, bitstream version 2, single stereo presentation DSI, and
    # distinguishable synthetic samples. Only the first 19 bits are AC-4
    # basic TOC fields; the rest is intentionally not decodable audio.
    dsi = bytes.fromhex("20a401400000001fffffffe0010ff88000004200000250100000030080")
    frames = []
    for seq in range(2 * SAMPLES_PER_SEGMENT):
        toc = (2 << 17) | (seq << 7) | (1 << 5) | (2 << 1) | 1
        body = bytes((seq + j) & 255 for j in range(221 + seq % 5))
        frames.append((toc << 5).to_bytes(3, "big") + body)
    return dsi, frames


def actual_fixture(path):
    info = probe(path, "-select_streams", "a:0", "-read_intervals", "0%+4",
                 "-show_entries", "stream=codec_tag_string,time_base:packet=pts,duration,pos,size")
    stream = info["streams"][0]
    check(stream["codec_tag_string"] == "ac-4" and stream["time_base"] == "1/48000",
          "expected a clear AC-4, 48 kHz local reference")
    blob = path.read_bytes()
    track_boxes = [body for tag, body in atoms(find_box(blob, b"moov")) if tag == b"trak"]
    entries = []
    for track in track_boxes:
        stsd = find_box(track, b"mdia", b"minf", b"stbl", b"stsd")
        entries.extend(body for tag, body in atoms(stsd[8:]) if tag == b"ac-4")
    check(len(entries) == 1, "fixture needs one clear AC-4 sample entry")
    dsi = find_box(entries[0][28:], b"dac4")
    frames = []
    for i, packet in enumerate(info["packets"][: 2 * SAMPLES_PER_SEGMENT]):
        check(int(packet["duration"]) == SAMPLE_DURATION and int(packet["pts"]) == i * SAMPLE_DURATION,
              f"expected continuous 25 fps audio at slot {i}")
        start, size = int(packet["pos"]), int(packet["size"])
        check(0 <= start < start + size <= len(blob), "reference sample offset out of bounds")
        frames.append(blob[start : start + size])
    check(len(frames) == 2 * SAMPLES_PER_SEGMENT, "reference must contain at least 4 seconds of audio")
    return dsi, frames


def audio_init(dsi):
    identity = struct.pack(">9I", 0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000)
    mvhd = full_atom(b"mvhd", struct.pack(">5Ih", 0, 0, 1000, 0, 0x10000, 0x100)
                      + bytes(10) + identity + bytes(24) + struct.pack(">I", 8))
    tkhd = full_atom(b"tkhd", struct.pack(">5I", 0, 0, AUDIO_TRACK, 0, 0) + bytes(8)
                      + struct.pack(">4H", 0, 0, 256, 0) + identity + bytes(8), flags=7)
    mdhd = full_atom(b"mdhd", struct.pack(">4I2H", 0, 0, TIMESCALE, 0, 0x55C4, 0))
    hdlr = full_atom(b"hdlr", bytes(4) + b"soun" + bytes(12) + b"SoundHandler\0")
    description = atom(b"ac-4", bytes(6) + struct.pack(">H", 1) + bytes(8)
                       + struct.pack(">HHHHI", 2, 16, 0, 0, TIMESCALE << 16) + atom(b"dac4", dsi))
    stsd = full_atom(b"stsd", struct.pack(">I", 1) + description)
    stbl = atom(b"stbl", stsd + full_atom(b"stts", bytes(4)) + full_atom(b"stsc", bytes(4))
                + full_atom(b"stsz", bytes(8)) + full_atom(b"stco", bytes(4)))
    dinf = atom(b"dinf", full_atom(b"dref", struct.pack(">I", 1) + full_atom(b"url ", flags=1)))
    minf = atom(b"minf", full_atom(b"smhd", bytes(4)) + dinf + stbl)
    trak = atom(b"trak", tkhd + atom(b"mdia", mdhd + hdlr + minf))
    trex = full_atom(b"trex", struct.pack(">5I", AUDIO_TRACK, 1, SAMPLE_DURATION, 0, 0))
    return atom(b"ftyp", b"iso6\x00\x00\x00\x01iso6mp41dash") + atom(b"moov", mvhd + trak + atom(b"mvex", trex))


def audio_traf(seq, samples, data_offset):
    base = (seq - 1) * SAMPLES_PER_SEGMENT * SAMPLE_DURATION
    tfhd = full_atom(b"tfhd", struct.pack(">2I", AUDIO_TRACK, SAMPLE_DURATION), flags=0x20008)
    tfdt = full_atom(b"tfdt", struct.pack(">Q", base), version=1)
    sizes = b"".join(struct.pack(">I", len(sample)) for sample in samples)
    trun = full_atom(b"trun", struct.pack(">Ii", len(samples), data_offset) + sizes, flags=0x201)
    return atom(b"traf", tfhd + tfdt + trun)


def audio_fragment(seq, samples):
    mfhd = full_atom(b"mfhd", struct.pack(">I", seq))

    def moof(data_offset):
        return atom(b"moof", mfhd + audio_traf(seq, samples, data_offset))

    return moof(len(moof(0)) + 8) + atom(b"mdat", b"".join(samples))


def publish_audio(upstream, prefix, dsi, samples):
    upstream.put(f"{prefix}/ac4-init.mp4", audio_init(dsi))
    for i in range(0, len(samples), SAMPLES_PER_SEGMENT):
        seq = i // SAMPLES_PER_SEGMENT + 1
        fragment = audio_fragment(seq, samples[i : i + SAMPLES_PER_SEGMENT])
        upstream.put(f"{prefix}/ac4-{seq:02d}.m4s", fragment)


def make_dash(upstream, root, dsi, samples):
    root = root / "dash"
    ffmpeg(root, 4, 40, ["-f", "dash", "-seg_duration", "2", "-use_template", "1",
                           "-use_timeline", "0", "manifest.mpd"])
    upstream.directory("/dash", root)
    publish_audio(upstream, "/dash", dsi, samples)
    tree = ET.fromstring((root / "manifest.mpd").read_text())
    namespace = tree.tag.split("}")[0] + "}"
    period = tree.find(namespace + "Period")
    adaptation = ET.SubElement(period, namespace + "AdaptationSet",
                               id="7", contentType="audio", lang="en", mimeType="audio/mp4")
    rep = ET.SubElement(adaptation, namespace + "Representation", id=AUDIO_ID,
                        mimeType="audio/mp4", codecs="ac-4.02.01.00", bandwidth="64000", audioSamplingRate="48000")
    ET.SubElement(rep, namespace + "SegmentTemplate", timescale=str(TIMESCALE), duration="96000",
                  media="ac4-$Number%02d$.m4s", initialization="ac4-init.mp4", startNumber="1")
    upstream.put("/dash/manifest.mpd", ET.tostring(tree))
    return upstream.origin + "/dash/manifest.mpd", AUDIO_ID


def make_hls(upstream, root, dsi, samples):
    ffmpeg(root, 4, 40, ["-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod",
                           "-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", "vinit.mp4",
                           "-hls_segment_filename", "video%03d.m4s", "video.m3u8"])
    upstream.directory("/hls", root)
    publish_audio(upstream, "/hls", dsi, samples)
    # Independent sequence origin, with the same VOD presentation extent.
    upstream.put("/hls/audio.m3u8", """#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:901
#EXT-X-MAP:URI="ac4-init.mp4"
#EXTINF:2.000,
ac4-01.m4s
#EXTINF:2.000,
ac4-02.m4s
#EXT-X-ENDLIST
""")
    upstream.put("/hls/master.m3u8", """#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE="en",URI="audio.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=500000,CODECS="avc1.42c00a,ac-4.02.01.00",AUDIO="aud"
video.m3u8
""")
    return upstream.origin + "/hls/master.m3u8", "hls-a-0"


def muxed_init(video_init, dsi):
    """Add exactly one audio track and its trex to the FFmpeg video-only init."""
    audio_moov = find_box(audio_init(dsi), b"moov")
    parts = []
    for kind, payload in atoms(find_box(video_init, b"moov")):
        if kind == b"mvhd":
            payload = payload[:-4] + struct.pack(">I", AUDIO_TRACK + 1)
        if kind == b"mvex":
            payload += atom(b"trex", find_box(audio_moov, b"mvex", b"trex"))
        parts.append(atom(kind, payload))
        if kind == b"trak":
            parts.append(atom(b"trak", find_box(audio_moov, b"trak")))
    return atom(b"ftyp", find_box(video_init, b"ftyp")) + atom(b"moov", b"".join(parts))


def muxed_fragment(video_fragment, seq, samples):
    """One moof, two trafs, and an mdat with the video then audio samples."""
    video_moof = find_box(video_fragment, b"moof")
    video_payload = find_box(video_fragment, b"mdat")
    video_traf = find_box(video_moof, b"traf")
    tfhd = find_box(video_traf, b"tfhd")
    flags = int.from_bytes(tfhd[1:4], "big")
    check(flags & 0x020001 == 0x020000, "fixture video defaults must be moof-relative")
    trun = find_box(video_traf, b"trun")
    check(trun[3] & 1, "fixture video trun needs data_offset")
    mfhd = atom(b"mfhd", find_box(video_moof, b"mfhd"))

    def moof(video_offset, audio_offset):
        parts = []
        for kind, payload in atoms(video_traf):
            if kind == b"trun":
                payload = payload[:8] + struct.pack(">i", video_offset) + payload[12:]
            parts.append(atom(kind, payload))
        return atom(b"moof", mfhd + atom(b"traf", b"".join(parts))
                    + audio_traf(seq, samples, audio_offset))

    video_offset = len(moof(0, 0)) + 8
    return (moof(video_offset, video_offset + len(video_payload))
            + atom(b"mdat", video_payload + b"".join(samples)))


def make_in_band_hls(upstream, root, dsi, samples):
    playlist = (root / "video.m3u8").read_text()
    upstream.put("/hls-in-band/vinit.mp4", muxed_init((root / "vinit.mp4").read_bytes(), dsi))
    names = playlist_urls(playlist)
    check(len(names) == 2, "expected two video segments to mux with the audio")
    for i, name in enumerate(names):
        first = i * SAMPLES_PER_SEGMENT
        video = (root / name).read_bytes()
        fragment = muxed_fragment(video, i + 1, samples[first : first + SAMPLES_PER_SEGMENT])
        upstream.put(f"/hls-in-band/{name}", fragment)
    upstream.put("/hls-in-band/video.m3u8", playlist)
    upstream.put("/hls-in-band/master.m3u8", """#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE="en"
#EXT-X-STREAM-INF:BANDWIDTH=500000,CODECS="avc1.42c00a,ac-4.02.01.00",AUDIO="aud"
video.m3u8
""")
    return upstream.origin + "/hls-in-band/master.m3u8", "hls-a-0"


def ts_payloads(data):
    check(len(data) % 188 == 0, "output TS packet alignment")
    for offset in range(0, len(data), 188):
        packet = data[offset : offset + 188]
        check(packet[0] == 0x47, "missing TS sync byte")
        control = (packet[3] >> 4) & 3
        if control not in (1, 3):
            continue
        start = 4 if control == 1 else 5 + packet[4]
        check(start <= len(packet), "bad TS adaptation field")
        if start < len(packet):
            yield ((packet[1] & 31) << 8) | packet[2], bool(packet[1] & 0x40), packet[start:]


def crc_mpeg2(data):
    crc = 0xffffffff
    for byte in data:
        crc ^= byte << 24
        for _ in range(8):
            crc = ((crc << 1) ^ (0x04C11DB7 if crc & 0x80000000 else 0)) & 0xffffffff
    return crc


def psi_section(packets, pid, table):
    payload = next(payload for p, first, payload in packets if p == pid and first)
    start = payload[0] + 1
    n = (((payload[start + 1] & 15) << 8) | payload[start + 2]) + 3
    section = payload[start : start + n]
    check(len(section) == n and section[0] == table and crc_mpeg2(section) == 0,
          f"invalid PSI section table {table}")
    return section


def tracks(data):
    packets = list(ts_payloads(data))
    pat = psi_section(packets, 0, 0)
    pmt_pid = next(int.from_bytes(pat[i + 2 : i + 4], "big") & 0x1FFF
                   for i in range(8, len(pat) - 4, 4) if pat[i : i + 2] != b"\0\0")
    pmt = psi_section(packets, pmt_pid, 2)
    offset = 12 + (int.from_bytes(pmt[10:12], "big") & 0xFFF)
    entries = []
    while offset < len(pmt) - 4:
        kind = pmt[offset]
        pid = int.from_bytes(pmt[offset + 1 : offset + 3], "big") & 0x1FFF
        length = int.from_bytes(pmt[offset + 3 : offset + 5], "big") & 0xFFF
        entries.append((kind, pid, pmt[offset + 5 : offset + 5 + length]))
        offset += 5 + length
    audios = [pid for kind, pid, desc in entries if kind == 0x06 and desc == PMT_DESCRIPTORS]
    videos = [pid for kind, pid, _ in entries if kind == 0x1B]
    check(len(audios) == len(videos) == 1 and len(entries) == 2,
          f"incorrect AC-4 PMT stream_type or descriptors: {entries!r}")
    return packets, audios[0], videos[0]


def pes_packets(packets, wanted_pid):
    sections = []
    for pid, first, payload in packets:
        if pid != wanted_pid:
            continue
        if first:
            sections.append(bytearray())
        check(bool(sections), "missing PES packet start")
        sections[-1].extend(payload)
    return sections


def pes_pts(pes):
    check(pes[:3] == b"\0\0\1" and pes[7] & 0x80 and pes[8] >= 5, "PES missing PTS")
    a, b, c, d, e = pes[9:14]
    check((a & c & e & 1) == 1, "bad PTS field markers")
    return ((a & 14) << 29) | (b << 22) | ((c & 254) << 14) | (d << 7) | (e >> 1)


def strip_sync(data):
    check(data.startswith(b"\xac\x40"), "expected a single AC40 sync frame")
    size = int.from_bytes(data[2:4], "big")
    header = 4
    if size == 0xffff:
        size = int.from_bytes(data[4:7], "big")
        header = 7
    check(size > 0 and len(data) == header + size, "incorrect AC4 sync frame size or extra wrapping")
    return data[header:]


def verify(gateway, url, audio_id, expected, label, *, probe_output=None):
    task = gateway.task(url, audio_rep_id=audio_id)
    try:
        gateway.finish(task)
        playlist = gateway.playlist(task)
        check("#EXT-X-ENDLIST" in playlist and abs(sum(playlist_times(playlist)) - 4) <= 0.001,
              f"wrong final extent: {playlist!r}")
        seen, audio_pts = [], []
        first_video_pts = None
        for index, entry in enumerate(playlist_urls(playlist)):
            segment = gateway.body(entry, auth=False)
            packets, audio, video = tracks(segment)
            if probe_output and index == 0:
                probe_output.write_bytes(segment)
                info = probe(probe_output, "-select_streams", "a:0", "-show_entries", "stream=codec_name")
                names = [stream.get("codec_name") for stream in info.get("streams", [])]
                check(names == ["ac4"], f"ffprobe did not identify output AC-4 audio: {names!r}")
            if first_video_pts is None:
                first_video_pts = pes_pts(pes_packets(packets, video)[0])
            for frame in pes_packets(packets, audio):
                check(frame[:4] == b"\x00\x00\x01\xbd", "AC-4 must use private_stream_1 PES")
                check(int.from_bytes(frame[4:6], "big") + 6 == len(frame),
                      "audio PES length does not match TS payload")
                check(frame[6] & 4, "AC-4 PES missing its data_alignment_indicator")
                check(frame[7] & 0xc0 == 0x80, "AC4 PES must carry PTS only")
                audio_pts.append(pes_pts(frame))
                seen.append(strip_sync(frame[9 + frame[8] :]))
        check(len(seen) == len(expected), f"AC-4 audio frame count {len(seen)} vs {len(expected)}")
        for i, (actual, wanted) in enumerate(zip(seen, expected)):
            check(actual == wanted, f"AC-4 raw frame {i} changed or extra sync header inserted")
        check(audio_pts[0] == first_video_pts, "zero-offset source audio and video origins diverged")
        check(all(b - a == 3600 for a, b in zip(audio_pts, audio_pts[1:])),
              "AC-4 25-fps PTS spacing changed")
        ffprobe_status = "; ffprobe=ac4" if probe_output else ""
        print(f"PASS {label}: {len(seen)} byte-identical AC-4 frames, complete PMT/PES, 25-fps PTS{ffprobe_status}")
    finally:
        gateway.delete(task)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/dv-hls-gateway")
    parser.add_argument("--reference-mp4", type=Path,
                        help="optional local clear Dolby test fixture (no network access)")
    parser.add_argument("--ffprobe-ac4", action="store_true",
                        help="require ffprobe with AC-4 TS demux support to identify output audio")
    args = parser.parse_args()
    dsi, frames = actual_fixture(args.reference_mp4) if args.reference_mp4 else synthetic_fixture()
    expected = list(frames)
    if not args.reference_mp4:
        # Some sources supply transport-framed samples despite the raw
        # sample contract; ensure a valid sync frame is not wrapped again.
        frames[4] = b"\xac\x40" + len(frames[4]).to_bytes(2, "big") + frames[4]
    with tempfile.TemporaryDirectory(prefix="dvhls-ac4-smoke-") as tmp, Upstream() as upstream:
        root = Path(tmp)
        hls_root = root / "hls"
        inputs = [
            (*make_dash(upstream, root, dsi, frames), "DASH"),
            (*make_hls(upstream, hls_root, dsi, frames), "HLS fMP4 (separate audio)"),
            (*make_in_band_hls(upstream, hls_root, dsi, frames), "HLS fMP4 (in-band audio)"),
        ]
        with Gateway(args.binary.resolve(), root / "gateway") as gateway:
            try:
                for url, audio, label in inputs:
                    verify(gateway, url, audio, expected, label,
                           probe_output=root / "probe.ts" if args.ffprobe_ac4 else None)
            except BaseException:
                print(gateway.log_tail(), file=sys.stderr)
                raise
    print("Output is a byte-level remux check, not a device playback certification.")


if __name__ == "__main__":
    main()
