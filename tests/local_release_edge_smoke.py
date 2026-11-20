#!/usr/bin/env python3
"""Reproduce the final review's timing and metadata cases over HTTP loopback.

Run after building: python3 tests/local_release_edge_smoke.py
Use --case to run individual cases, and --binary to check another build.
All sources are synthesized locally; FFmpeg/libx264, ffprobe and Python 3 are required.
"""

import argparse
import re
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

sys.dont_write_bytecode = True
from local_ac4_smoke import (
    AUDIO_ID, SAMPLES_PER_SEGMENT, atom, atoms, find_box, make_dash, muxed_fragment,
    muxed_init, pes_packets, pes_pts, publish_audio, strip_sync, synthetic_fixture,
    tracks,
)
from local_gateway_smoke import (
    Gateway, REPO, Upstream, check, ffmpeg, playlist_times, playlist_urls, probe,
    run_media_tool,
)


def hls_video(folder, seconds):
    ffmpeg(folder, seconds, 40, [
        "-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod",
        "-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", "vinit.mp4",
        "-hls_segment_filename", "video%03d.m4s", "video.m3u8",
    ])
    return playlist_urls((folder / "video.m3u8").read_text())


def fmp4_media(init, names, *, sequence=0, vod=False):
    lines = ["#EXTM3U", "#EXT-X-VERSION:7", "#EXT-X-TARGETDURATION:2",
             f"#EXT-X-MEDIA-SEQUENCE:{sequence}", f'#EXT-X-MAP:URI="{init}"']
    if vod:
        lines.append("#EXT-X-PLAYLIST-TYPE:VOD")
    for name in names:
        lines.extend(("#EXTINF:2.000,", name))
    lines.append("#EXT-X-ENDLIST")
    return "\n".join(lines) + "\n"


def read_ts(gateway, task, *, media_suffix="", extent=4):
    gateway.finish(task)
    playlist = gateway.playlist(task, media_suffix)
    check("#EXT-X-ENDLIST" in playlist, f"missing final ENDLIST: {playlist!r}")
    check(abs(sum(playlist_times(playlist)) - extent) < 0.002,
          f"wrong output playlist extent: {playlist!r}")
    segments = [gateway.body(url, auth=False) for url in playlist_urls(playlist)]
    check(bool(segments), "no output TS segments")
    return segments


def audio_video_ts(segments):
    frames, audio_pts, video_pts = [], [], []
    for segment in segments:
        packets, audio, video = tracks(segment)
        video_pts.extend(pes_pts(packet) for packet in pes_packets(packets, video))
        for frame in pes_packets(packets, audio):
            audio_pts.append(pes_pts(frame))
            frames.append(strip_sync(frame[9 + frame[8] :]))
    check(bool(audio_pts) and bool(video_pts), "output lost selected audio or video")
    return frames, audio_pts, video_pts


def check_frames(actual, expected):
    check(len(actual) == len(expected), f"expected {len(expected)} AC-4 frames, got {len(actual)}")
    for i, (actual_frame, expected_frame) in enumerate(zip(actual, expected)):
        check(actual_frame == expected_frame, f"wrong source audio payload at frame {i}")


def signed_pts_delta(a, b):
    return ((a - b + (1 << 32)) % (1 << 33)) - (1 << 32)


def check_pts_steps(timestamps, duration):
    check(all(signed_pts_delta(b, a) == duration for a, b in zip(timestamps, timestamps[1:])),
          f"output no longer has continuous {duration}-tick PTS spacing")


def hls_different_endlist_windows(gateway, upstream, folder):
    # One extra 2s period of audio in the final playlist. The overlap is
    # exact, but neither ENDLIST nor matching elapsed EXTINF identifies it.
    video_names = hls_video(folder, 6)
    check(len(video_names) == 3, "expected three source video segments")
    dsi, frames = synthetic_fixture()
    leading = list(reversed(frames[:SAMPLES_PER_SEGMENT]))
    upstream.directory("/finite", folder)
    publish_audio(upstream, "/finite", dsi, [*leading, *frames])
    upstream.put("/finite/audio.m3u8", fmp4_media("ac4-init.mp4", [
        "ac4-01.m4s", "ac4-02.m4s", "ac4-03.m4s",
    ], sequence=101))
    upstream.put("/finite/master.m3u8", """#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE="en",URI="audio.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=500000,CODECS="avc1.42c00a,ac-4.02.01.00",AUDIO="aud"
finite.m3u8
""")
    for video_sequence in (102, 902):
        upstream.put("/finite/finite.m3u8", fmp4_media("vinit.mp4", video_names[1:],
                                                        sequence=video_sequence))
        task = gateway.task(upstream.origin + "/finite/master.m3u8", audio_rep_id="hls-a-0")
        try:
            actual, audio_pts, video_pts = audio_video_ts(read_ts(gateway, task))
            check_frames(actual, frames)
            check_pts_steps(audio_pts, 3600)
            check(signed_pts_delta(audio_pts[0], video_pts[0]) == 0,
                  f"aligned finite rendition windows did not start together ({video_sequence=})")
        finally:
            gateway.delete(task)


def bump_tfdt(video, delta):
    def transform_traf(traf):
        children = []
        for kind, payload in atoms(traf):
            if kind == b"tfdt":
                size = 8 if payload[0] == 1 else 4
                check(len(payload) == 4 + size, "unexpected tfdt layout")
                timestamp = int.from_bytes(payload[4:], "big")
                payload = payload[:4] + (timestamp + delta).to_bytes(size, "big")
            children.append(atom(kind, payload))
        return b"".join(children)

    return b"".join(atom(kind, b"".join(atom(t, transform_traf(p) if t == b"traf" else p)
                                       for t, p in atoms(payload)) if kind == b"moof" else payload)
                    for kind, payload in atoms(video))


def hls_in_band_audio_lead(gateway, upstream, folder):
    names = hls_video(folder, 4)
    check(len(names) == 2, "expected two source video segments")
    dsi, frames = synthetic_fixture()
    init = (folder / "vinit.mp4").read_bytes()
    mdhd = find_box(init, b"moov", b"trak", b"mdia", b"mdhd")
    timescale_offset = 20 if mdhd[0] == 1 else 12
    video_timescale = struct.unpack_from(">I", mdhd, timescale_offset)[0]
    check(video_timescale % 10 == 0, "fixture needs exact 100 ms in the video timebase")
    upstream.put("/lead/vinit.mp4", muxed_init(init, dsi))
    for i, name in enumerate(names):
        start = i * SAMPLES_PER_SEGMENT
        source = bump_tfdt((folder / name).read_bytes(), video_timescale // 10)
        fragment = muxed_fragment(source, i + 1, frames[start : start + SAMPLES_PER_SEGMENT])
        upstream.put(f"/lead/{name}", fragment)
    upstream.put("/lead/index.m3u8", fmp4_media("vinit.mp4", names, vod=True))
    task = gateway.task(upstream.origin + "/lead/index.m3u8")
    try:
        actual, audio_pts, video_pts = audio_video_ts(read_ts(gateway, task))
        check_frames(actual, frames)
        check_pts_steps(audio_pts, 3600)
        check_pts_steps(video_pts, 4500)
        for audio_index, video_index in ((0, 0), (50, 40)):
            difference = signed_pts_delta(audio_pts[audio_index], video_pts[video_index])
            check(difference == -9000, f"expected AC-4 lead of 100 ms, got {-difference / 90:g} ms")
    finally:
        gateway.delete(task)


def generate_dash_priming(source, codec, *, key_interval=40):
    source.mkdir(parents=True)
    run_media_tool("ffmpeg", [
        "-nostdin", "-y", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25",
        "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "3.2",
        "-c:v", "libx264", "-preset", "ultrafast", "-threads", "1", "-bf", "0",
        "-g", str(key_interval), "-keyint_min", str(key_interval), "-sc_threshold", "0",
        "-c:a", codec, "-b:a", "128k", "-f", "dash", "-seg_duration", "1.6",
        "-use_timeline", "1", "index.mpd",
    ], cwd=source)
    audio_input = source.parent / "audio-source.mp4"
    audio_input.write_bytes((source / "init-stream1.m4s").read_bytes()
                            + sorted(source.glob("chunk-stream1-*.m4s"))[0].read_bytes())
    encoded = probe(audio_input, "-select_streams", "a:0", "-read_intervals", "%+#1",
                    "-show_entries", "packet=pts,duration")["packets"][0]
    check(int(encoded["pts"]) < 0 < int(encoded["pts"]) + int(encoded["duration"]),
          f"FFmpeg's {codec} fixture did not straddle the Period origin: {encoded!r}")


def dash_audio_encoder_priming(gateway, upstream, folder):
    for codec in ("ac3", "eac3"):
        source = folder / codec
        generate_dash_priming(source, codec)
        prefix = f"/priming-{codec}"
        upstream.directory(prefix, source)
        task = gateway.task(upstream.origin + prefix + "/index.mpd", audio_rep_id="1")
        try:
            combined = folder / f"{codec}.ts"
            combined.write_bytes(b"".join(read_ts(gateway, task, extent=3.2)))
            output = probe(combined, "-show_entries", "stream=codec_type,codec_name:packet=stream_index,pts,duration")
            video = next((i for i, s in enumerate(output["streams"]) if s["codec_type"] == "video"), None)
            audio = next((i for i, s in enumerate(output["streams"]) if s["codec_type"] == "audio"), None)
            check(audio is not None and video is not None and output["streams"][audio]["codec_name"] == codec,
                  f"output did not retain {codec}: {output.get('streams')!r}")
            pts = [int(p["pts"]) for p in output["packets"] if p["stream_index"] == audio]
            frame_ticks = 2880  # 1536 samples at 48 kHz in the 90 kHz TS clock
            first_video = next(int(p["pts"]) for p in output["packets"] if p["stream_index"] == video)
            check(abs(len(pts) - 100) <= 1 and abs(signed_pts_delta(pts[0], first_video)) <= frame_ticks,
                  f"{codec} output changed duration or A/V origin after accounting for the priming frame")
            check_pts_steps(pts, frame_ticks)
            run_media_tool("ffmpeg", ["-nostdin", "-xerror", "-i", str(combined), "-map", "0:a:0", "-f", "null", "-"])
        finally:
            gateway.delete(task)


def dash_audio_real_crop(gateway, upstream, folder):
    source = folder / "source"
    # Video can start cleanly at 40 ms (the next sample is an IDR);
    # this is inside the second 32 ms AC-3 AU, not its initial priming.
    generate_dash_priming(source, "ac3", key_interval=1)
    manifest = ET.fromstring((source / "index.mpd").read_text())
    manifest.set("mediaPresentationDuration", "PT3.16S")
    manifest.find("{*}Period").set("duration", "PT3.16S")
    for template in manifest.findall(".//{*}SegmentTemplate"):
        timescale = int(template.get("timescale"))
        check(timescale % 25 == 0, "fixture needs exactly 40 ms in each timebase")
        template.set("presentationTimeOffset", str(timescale // 25))
    upstream.directory("/crop", source)
    upstream.put("/crop/index.mpd", ET.tostring(manifest))
    task = gateway.task(upstream.origin + "/crop/index.mpd", audio_rep_id="1")
    try:
        status = gateway.wait_state(task, "finished", "error")
        check(status["state"] == "error" and "partial leading crop" in status.get("error", ""),
              f"true audio crop was accepted or rejected for the wrong reason: {status!r}")
    finally:
        gateway.delete(task)


def order_init_tracks(init, audio_first):
    def order_moov(moov):
        boxes = list(atoms(moov))
        positions = [i for i, (tag, _) in enumerate(boxes) if tag == b"trak"]
        check(len(positions) == 2, "shared init fixture needs one audio and one video track")
        ordered = sorted((boxes[i] for i in positions),
                         key=lambda track: find_box(track[1], b"mdia", b"hdlr")[8:12] == b"soun",
                         reverse=audio_first)
        for i, track in zip(positions, ordered):
            boxes[i] = track
        return b"".join(atom(kind, payload) for kind, payload in boxes)

    return b"".join(atom(kind, order_moov(payload) if kind == b"moov" else payload)
                    for kind, payload in atoms(init))


def dash_shared_init_track_order(gateway, upstream, folder):
    dsi, frames = synthetic_fixture()
    url, audio_id = make_dash(upstream, folder, dsi, frames)
    check(audio_id == AUDIO_ID, "unexpected fixture audio representation")
    dash_root = folder / "dash"
    init = next(dash_root.glob("init-stream*.m4s"))
    shared = muxed_init(init.read_bytes(), dsi)
    # Both representations point at the same init, but their fragments
    # remain single-track and retain the original, very different clocks.
    manifest = ET.fromstring(upstream.response("/dash/manifest.mpd")[2])
    for template in manifest.findall(".//{*}SegmentTemplate"):
        template.set("initialization", "shared-init.m4s")
    upstream.put("/dash/manifest.mpd", ET.tostring(manifest))
    results = []
    for audio_first in (False, True):
        upstream.put("/dash/shared-init.m4s", order_init_tracks(shared, audio_first))
        task = gateway.task(url, audio_rep_id=audio_id)
        try:
            actual, audio_pts, video_pts = audio_video_ts(read_ts(gateway, task))
            check_frames(actual, frames)
            check(len(video_pts) == 80, f"shared init emitted {len(video_pts)} video frames instead of 80")
            check_pts_steps(video_pts, 4500)
            check_pts_steps(audio_pts, 3600)
            results.append((audio_pts, video_pts))
        finally:
            gateway.delete(task)
    check(results[0] == results[1], "changing trak order altered the selected timeline")


def hls_selected_audio_codecs(gateway, upstream, folder):
    names = hls_video(folder, 4)
    dsi, frames = synthetic_fixture()
    upstream.directory("/codecs", folder)
    publish_audio(upstream, "/codecs", dsi, frames)
    upstream.put("/codecs/audio.m3u8", fmp4_media("ac4-init.mp4", ["ac4-01.m4s", "ac4-02.m4s"], vod=True))
    aac = folder / "aac"
    aac.mkdir()
    run_media_tool("ffmpeg", [
        "-nostdin", "-y", "-f", "lavfi", "-i", "sine=frequency=880:sample_rate=48000", "-t", "4",
        "-c:a", "aac", "-b:a", "64k", "-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod",
        "-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", "init.mp4", "index.m3u8",
    ], cwd=aac)
    upstream.directory("/codecs/aac", aac)
    upstream.put("/codecs/video.m3u8", fmp4_media("vinit.mp4", names, vod=True))
    upstream.put("/codecs/text.m3u8", """#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-PLAYLIST-TYPE:VOD
#EXTINF:2.000,
first.vtt
#EXTINF:2.000,
second.vtt
#EXT-X-ENDLIST
""")
    for name in ("first", "second"):
        upstream.put(f"/codecs/{name}.vtt", f"WEBVTT\n\n00:00:00.100 --> 00:00:01.000\n{name}\n")
    upstream.put("/codecs/master.m3u8", """#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="AC-4",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE="en",URI="audio.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="AAC",DEFAULT=NO,AUTOSELECT=YES,LANGUAGE="fr",URI="aac/index.m3u8"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="sub",NAME="Captions",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE="en",URI="text.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=500000,CODECS="avc1.42c00a,ac-4.02.01.00,mp4a.40.2",AUDIO="aud",SUBTITLES="sub"
video.m3u8
""")
    for n, (selected, other) in enumerate((("ac-4", "mp4a"), ("mp4a", "ac-4"))):
        task = gateway.task(upstream.origin + "/codecs/master.m3u8", audio_rep_id=f"hls-a-{n}",
                            enable_subtitles=True)
        try:
            segments = read_ts(gateway, task, media_suffix="/api")
            if selected == "ac-4":
                actual, _, _ = audio_video_ts(segments)
                check_frames(actual, frames)
            else:
                transport = folder / "audio-output.ts"
                transport.write_bytes(segments[0])
                audio = probe(transport, "-select_streams", "a:0", "-show_entries", "stream=codec_name")
                check([stream["codec_name"] for stream in audio["streams"]] == ["aac"],
                      f"expected only selected AAC output: {audio!r}")
            master = gateway.playlist(task)
            line = next((line for line in master.splitlines() if line.startswith("#EXT-X-STREAM-INF:")), "")
            check('SUBTITLES="subs"' in line, "expected subtitle master")
            if re.search(r"[:,]CODECS=", line):
                codecs = re.search(r'[:,]CODECS="([^"]*)"(?:,|$)', line)
                check(codecs is not None, f"malformed CODECS: {line!r}")
                tokens = [token.strip().lower().split(".", 1)[0] for token in codecs.group(1).split(",")]
                check(selected in tokens and other not in tokens and ("avc1" in tokens or "avc3" in tokens),
                      f"master did not describe only selected audio ({selected}): {tokens!r}")
        finally:
            gateway.delete(task)


CASES = {
    "hls-endlist-windows": hls_different_endlist_windows,
    "hls-inband-lead": hls_in_band_audio_lead,
    "dash-audio-priming": dash_audio_encoder_priming,
    "dash-real-crop": dash_audio_real_crop,
    "dash-shared-init": dash_shared_init_track_order,
    "hls-selected-codecs": hls_selected_audio_codecs,
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/dv-hls-gateway")
    parser.add_argument("--case", action="append", choices=CASES)
    args = parser.parse_args()
    cases = list(dict.fromkeys(args.case or CASES))
    failures = 0
    with tempfile.TemporaryDirectory(prefix="dvhls-release-edges-") as tmp, Upstream() as upstream:
        root = Path(tmp)
        with Gateway(args.binary.resolve(), root / "gateway") as gateway:
            for name in cases:
                try:
                    CASES[name](gateway, upstream, root / name)
                    print(f"PASS {name}", flush=True)
                except (AssertionError, OSError, KeyError, ValueError, subprocess.SubprocessError) as error:
                    failures += 1
                    print(f"FAIL {name}: {error}", flush=True)
                    if isinstance(error, subprocess.CalledProcessError) and error.stderr:
                        print(error.stderr.decode(errors="replace"), flush=True)
    print(f"{len(cases) - failures}/{len(cases)} release-edge loopback cases passed", flush=True)
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())
