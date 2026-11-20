#!/usr/bin/env python3
"""End-to-end tests with FFmpeg-generated clear media and loopback HTTP only.

Run on a freshly built gateway:
  cargo build --locked --offline --release
  python3 tests/local_gateway_smoke.py --binary target/release/dv-hls-gateway
  python3 tests/local_gateway_smoke.py --list

Requires ffmpeg (libx264 + AAC), ffprobe and Python 3.10+. Temporary fixtures,
listener ports, config and management credentials are created per run. This
is deliberately outside `cargo test`, whose unit suite needs no FFmpeg.
"""

import argparse
import collections
import concurrent.futures
import contextlib
import copy
import http.server
import json
import os
from pathlib import Path
import re
import shutil
import socket
import stat
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET


HOST = "127.0.0.1"
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
REPO = Path(__file__).resolve().parents[1]


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def eventually(predicate, description, timeout=15):
    deadline = time.monotonic() + timeout
    while True:
        result = predicate()
        if result:
            return result
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {description}")
        time.sleep(0.08)


def run_media_tool(program, arguments, cwd=None):
    return subprocess.run(
        [program, "-v", "error", *arguments], cwd=cwd,
        check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30,
    ).stdout


def ffmpeg(directory, seconds, key_interval, output, *, audio=False):
    directory.mkdir(parents=True, exist_ok=True)
    args = ["-nostdin", "-y", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=20"]
    if audio:
        args += ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"]
    args += [
        "-t", str(seconds), "-pix_fmt", "yuv420p", "-c:v", "libx264", "-preset", "ultrafast",
        "-threads", "1", "-bf", "0", "-g", str(key_interval), "-keyint_min", str(key_interval),
        "-sc_threshold", "0",
    ]
    if audio:
        args += ["-c:a", "aac", "-b:a", "64k"]
    return run_media_tool("ffmpeg", args + output, cwd=directory)


def probe(path, *args):
    return json.loads(run_media_tool("ffprobe", [*args, "-of", "json", str(path)]))


def media_playlist(segments, *, final=False, sequence=100, discontinuities=()):
    lines = ["#EXTM3U", "#EXT-X-VERSION:3", "#EXT-X-TARGETDURATION:4",
             f"#EXT-X-MEDIA-SEQUENCE:{sequence}"]
    for n, name in enumerate(segments):
        if n in discontinuities:
            lines.append("#EXT-X-DISCONTINUITY")
        lines.extend(("#EXTINF:4.000,", name))
    if final:
        lines.append("#EXT-X-ENDLIST")
    return "\n".join(lines) + "\n"


def playlist_urls(text):
    return [line.strip() for line in text.splitlines() if line.strip() and not line.startswith("#")]


def playlist_times(text):
    return [float(n) for n in re.findall(r"^#EXTINF:([0-9.]+),", text, re.MULTILINE)]


class Upstream:
    def __init__(self):
        self.lock = threading.Lock()
        self.responses = {}
        self.calls = collections.Counter()

    def put(self, path, content, status=200, headers=(), gate=None):
        if isinstance(content, str):
            content = content.encode()
        with self.lock:
            self.responses[path] = (status, tuple(headers), content, gate)

    def redirect(self, path, target):
        check(target.startswith("/") and not target.startswith("//"), "fixture redirect is not local")
        self.put(path, b"", 302, [("Location", target)])

    def directory(self, prefix, folder):
        for file in folder.iterdir():
            if file.is_file():
                self.put(f"{prefix}/{file.name}", file.read_bytes())

    def count(self, target):
        with self.lock:
            return self.calls[target]

    def response(self, target):
        with self.lock:
            self.calls[target] += 1
            return self.responses.get(target, self.responses.get(
                urllib.parse.urlsplit(target).path, (404, (), b"unknown fixture", None),
            ))

    def __enter__(self):
        upstream = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                status, headers, content, gate = upstream.response(self.path)
                if gate is not None:
                    gate.wait(timeout=10)
                self.send_response(status)
                for key, value in headers:
                    self.send_header(key, value)
                self.send_header("Content-Length", str(len(content)))
                self.end_headers()
                try:
                    self.wfile.write(content)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # Expected when the gateway intentionally cancels a request.

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer((HOST, 0), Handler)
        self.server.daemon_threads = True
        self.origin = f"http://{HOST}:{self.server.server_port}"
        self.worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.worker.start()
        return self

    def __exit__(self, *_):
        self.server.shutdown()
        self.server.server_close()
        self.worker.join(timeout=3)


class Gateway:
    def __init__(self, binary, directory, **env):
        self.binary = binary
        self.directory = directory
        # Ensure proxy config inherited from a developer machine cannot route loopback traffic.
        self.env = {key: value for key, value in os.environ.items() if not key.lower().endswith("proxy")}
        for key in list(self.env):
            if key.startswith(("DVHLS_OUTPUT_", "MPD_HLS_")):
                del self.env[key]
        self.env.update(RUST_LOG="info", **env)
        self.key = None
        self.proc = None

    def __enter__(self):
        self.directory.mkdir(parents=True, exist_ok=True)
        self.log = self.directory / "gateway.log"
        self.config = self.directory / "config.json"
        with socket.socket() as s:
            s.bind((HOST, 0))
            port = s.getsockname()[1]
        self.origin = f"http://{HOST}:{port}"
        options = {"umask": 0o022} if os.name == "posix" else {}
        try:
            with self.log.open("w") as out:
                self.proc = subprocess.Popen(
                    [str(self.binary), "--config", str(self.config), "--port", str(port)],
                    cwd=self.directory, stdout=out, stderr=subprocess.STDOUT, env=self.env, **options,
                )

            def ready():
                check(self.proc.poll() is None, f"gateway exited {self.proc.returncode}: {self.log_tail()}")
                try:
                    conf = json.loads(self.config.read_text())
                    self.key = conf["auth"]["key"]
                    return self.request("/api/tasks")[0] == 200
                except (FileNotFoundError, urllib.error.URLError):
                    return False

            eventually(ready, "gateway startup", timeout=10)
            return self
        except BaseException:
            self.close()
            raise

    def __exit__(self, *_):
        self.close()

    def close(self):
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=3)

    def log_tail(self):
        return "\n".join(self.log.read_text(errors="replace").splitlines()[-45:])

    def request(self, path, *, method=None, payload=None, auth=True):
        # Accept only gateway URLs on the same loopback listener.
        if not path.startswith("/"):
            target = urllib.parse.urlsplit(path)
            check(f"{target.scheme}://{target.netloc}" == self.origin, "non-loopback gateway media URL")
            path = target.path + (f"?{target.query}" if target.query else "")
        check(not path.startswith("//"), "ambiguous gateway request target")
        data = None if payload is None else json.dumps(payload).encode()
        headers = {"Content-Type": "application/json"} if payload is not None else {}
        if auth:
            headers["X-Auth-Key"] = self.key
        req = urllib.request.Request(self.origin + path, data=data, method=method, headers=headers)
        try:
            with OPENER.open(req, timeout=9) as resp:
                return resp.status, resp.headers, resp.read()
        except urllib.error.HTTPError as error:
            with error:
                return error.code, error.headers, error.read()

    def body(self, path, **kwargs):
        status, _, body = self.request(path, **kwargs)
        check(status == 200, f"{path}: expected 200, got {status}: {body[:300]!r}")
        return body

    def create(self, url, track=None, **settings):
        info = json.loads(self.body("/api/parse", payload={"mpd": url}))
        check(bool(info["video"]), f"/api/parse had no video: {info!r}")
        return self.create_response(url, track or info["video"][0]["id"], **settings)

    def create_response(self, url, track, **settings):
        payload = {"mpd": url, "video_rep_id": track, "key_mode": "clear", **settings}
        return self.request("/api/tasks", payload=payload)

    def task(self, url, **settings):
        code, _, body = self.create(url, **settings)
        check(200 <= code < 300, f"create {url}: {code} {body[:300]!r}")
        return json.loads(body)

    def status(self, task):
        all_tasks = json.loads(self.body("/api/tasks"))
        return next(entry for entry in all_tasks if entry["id"] == task["id"])

    def wait_state(self, task, *states):
        def found():
            status = self.status(task)
            return status if status["state"] in states else None
        return eventually(found, f"{task['id']} in {states}")

    def finish(self, task):
        status = self.wait_state(task, "finished", "error")
        check(status["state"] == "finished", f"task failed: {status}")
        check(status["segments_available"] > 0, f"finished with no output: {status}")
        return status

    def playlist(self, task, suffix=""):
        return self.body(task["playlist_url"] + suffix, auth=False).decode()

    def delete(self, task):
        self.request(f"/api/tasks/{task['id']}", method="DELETE")


class Fixtures:
    def __init__(self, directory, upstream):
        self.directory, self.upstream = directory, upstream
        self.ts_data = None

    def dash(self, name, seconds, *, fragmented=False, key_interval=40):
        folder = self.directory / name
        options = ["-f", "dash", "-seg_duration", "2", "-use_template", "1", "-use_timeline", "0"]
        if fragmented:
            options += ["-frag_type", "duration", "-frag_duration", "0.5"]
        ffmpeg(folder, seconds, key_interval, options + ["manifest.mpd"])
        self.upstream.directory(f"/{name}", folder)
        return folder

    def fmp4(self):
        folder = self.directory / "native-fmp4"
        ffmpeg(folder, 5.5, 40, ["-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod",
                                  "-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", "init.mp4",
                                  "-hls_segment_filename", "media%03d.m4s", "index.m3u8"], audio=True)
        self.upstream.directory("/fmp4/media", folder)

    def ts(self):
        if self.ts_data is None:
            folder = self.directory / "native-ts"
            ffmpeg(folder, 16, 80, ["-f", "hls", "-hls_time", "4", "-hls_playlist_type", "vod",
                                     "-hls_segment_filename", "video%03d.ts", "index.m3u8"])
            self.ts_paths = sorted(folder.glob("*.ts"))
            self.ts_data = [p.read_bytes() for p in self.ts_paths]
            check(len(self.ts_data) == 4, "expected exactly four synthetic TS chunks")
        return self.ts_data

    def probe_output(self, gateway, playlist):
        outputs = []
        for n, url in enumerate(playlist_urls(playlist)):
            dest = self.directory / f"output-{n}.ts"
            dest.write_bytes(gateway.body(url, auth=False))
            outputs += probe(dest, "-count_packets", "-show_entries",
                             "stream=codec_type,nb_read_packets,duration").get("streams", [])
        return outputs


def assert_vod(gateway, task, seconds, *, video_frames, audio=False, fixtures):
    gateway.finish(task)
    playlist = gateway.playlist(task)
    check("#EXT-X-ENDLIST" in playlist, f"finished VOD has no ENDLIST: {playlist!r}")
    duration = sum(playlist_times(playlist))
    check(abs(duration - seconds) <= 0.052, f"incorrect VOD duration: {duration} vs {seconds}")
    outputs = fixtures.probe_output(gateway, playlist)
    packets = sum(int(o["nb_read_packets"]) for o in outputs if o["codec_type"] == "video")
    check(packets == video_frames, f"incorrect video packet count: {packets} vs {video_frames}")
    if audio:
        check(any(o["codec_type"] == "audio" and int(o["nb_read_packets"]) > 10 for o in outputs),
              f"muxed input lost its audio: {outputs!r}")


def dash_short_tail(gateway, fixtures):
    f = fixtures.dash("dash-short", 5.5)
    text = (f / "manifest.mpd").read_text()
    check("$Number%05d$" in text, "FFmpeg DASH fixture unexpectedly lacks default-formatted segment number")
    u = fixtures.upstream
    u.redirect("/dash-entry.mpd", "/dash-short/manifest.mpd")
    task = gateway.task(u.origin + "/dash-entry.mpd")
    try:
        assert_vod(gateway, task, 5.5, video_frames=110, fixtures=fixtures)
        init = next(f.glob("init-stream*")).name
        check(u.count(f"/dash-short/{init}") >= 1 and u.count(f"/{init}") == 0,
              "DASH initialization ignored the effective manifest URL")
    finally:
        gateway.delete(task)


def dash_track_isolation(gateway, fixtures):
    folder = fixtures.dash("dash-track", 4)
    xml = ET.fromstring((folder / "manifest.mpd").read_text())
    period = xml.find("{*}Period")
    original = period.find("{*}AdaptationSet")
    bad = copy.deepcopy(original)
    bad.set("id", "invalid-adaptation")
    bad.find("{*}Representation").set("id", "bad-track")
    bad.find(".//{*}SegmentTemplate").set("media", "media-$NotAnIdentifier$.m4s")
    period.append(bad)
    path = "/dash-track/isolated.mpd"
    fixtures.upstream.put(path, ET.tostring(xml, encoding="unicode"))
    url = fixtures.upstream.origin + path
    info = json.loads(gateway.body("/api/parse", payload={"mpd": url}))
    good_id = original.find("{*}Representation").get("id")
    check([track["id"] for track in info["video"]] == [good_id],
          f"unsupported representation poisoned valid alternatives: {info!r}")
    status, _, response = gateway.create_response(url, "bad-track")
    check(status == 400 and b"bad-track" in response and b"SegmentTemplate" in response,
          f"explicit unsupported track did not return a track-specific diagnostic: {status} {response!r}")
    task = gateway.task(url)
    try:
        assert_vod(gateway, task, 4.0, video_frames=80, fixtures=fixtures)
    finally:
        gateway.delete(task)


def dash_multiple_moof(gateway, fixtures):
    folder = fixtures.dash("dash-moof", 4, fragmented=True)
    media = sorted(folder.glob("chunk-stream*"))
    check(len(media) == 2 and all(path.read_bytes().count(b"moof") > 1 for path in media),
          "FFmpeg did not create multiple movie fragments per media segment")
    task = gateway.task(fixtures.upstream.origin + "/dash-moof/manifest.mpd")
    try:
        assert_vod(gateway, task, 4.0, video_frames=80, fixtures=fixtures)
    finally:
        gateway.delete(task)


def period_extent_manifest(folder):
    xml = ET.fromstring((folder / "manifest.mpd").read_text())
    xml.set("mediaPresentationDuration", "PT4.5S")
    period = xml.find("{*}Period")
    period.set("duration", "PT4.5S")
    for template in xml.findall(".//{*}SegmentTemplate"):
        ticks_per_second = template.get("timescale", "1")
        template.set("presentationTimeOffset", ticks_per_second)
        template.set("eptDelta", f"-{ticks_per_second}")
    video = period.find(".//{*}Representation").get("id")
    return ET.tostring(xml, encoding="unicode"), video


def dash_period_extent(gateway, fixtures):
    f = fixtures.dash("dash-period", 6.0)
    path = "/dash-period/extent.mpd"
    manifest, video = period_extent_manifest(f)
    fixtures.upstream.put(path, manifest)
    url = fixtures.upstream.origin + path
    code, _, body = gateway.create_response(url, video)
    if code == 400:
        check(re.search(rb"clip|period|crop|extent|boundary|keyframe", body, re.I),
              f"Period fixture was rejected for an unrelated reason: {body!r}")
        return
    check(200 <= code < 300, f"unexpected Period create response: {code} {body!r}")
    task = json.loads(body)
    try:
        status = gateway.wait_state(task, "finished", "error")
        if status["state"] == "error":
            check(re.search(r"clip|period|crop|extent|boundary|keyframe", status.get("error", ""), re.I),
                  f"Period fixture failed for an unrelated reason: {status!r}")
            code, _, body = gateway.request(task["playlist_url"], auth=False)
            check(code == 503 or (code == 200 and b"#EXT-X-ENDLIST" not in body),
                  "failed Period exposed a false complete playlist")
        else:
            assert_vod(gateway, task, 4.5, video_frames=90, fixtures=fixtures)
    finally:
        gateway.delete(task)


def dash_aligned_crop(gateway, fixtures):
    # One-second IDR interval, two-second physical segments. Period starts
    # one second into the first segment and ends between the last keyframes.
    f = fixtures.dash("dash-aligned-crop", 6.0, key_interval=20)
    path = "/dash-aligned-crop/extent.mpd"
    manifest, _ = period_extent_manifest(f)
    fixtures.upstream.put(path, manifest)
    task = gateway.task(fixtures.upstream.origin + path)
    try:
        assert_vod(gateway, task, 4.5, video_frames=90, fixtures=fixtures)
    finally:
        gateway.delete(task)


def dash_live_endlist(gateway, fixtures):
    f = fixtures.dash("dash-ending", 5.5)
    path = "/dash-ending/live.mpd"
    finite = (f / "manifest.mpd").read_bytes()
    dynamic = finite.replace(b'type="static"', b'type="dynamic"', 1)
    check(dynamic != finite, "invalid DASH live fixture")
    u = fixtures.upstream
    u.put(path, dynamic)
    task = gateway.task(u.origin + path)
    try:
        eventually(lambda: u.count(path) >= 3, "initial worker MPD request")
        check(gateway.status(task)["is_dynamic"], "expected live DASH task before EOF")
        u.put(path, finite)
        assert_vod(gateway, task, 5.5, video_frames=110, fixtures=fixtures)
    finally:
        gateway.delete(task)


def hls_fmp4_redirect(gateway, fixtures):
    fixtures.fmp4()
    u = fixtures.upstream
    u.put("/fmp4/master/index.m3u8", '#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=900000,CODECS="avc1.42E01E,mp4a.40.2"\nchild.m3u8\n')
    u.redirect("/fmp4/entry.m3u8", "/fmp4/master/index.m3u8")
    u.redirect("/fmp4/master/child.m3u8", "/fmp4/media/index.m3u8")
    task = gateway.task(u.origin + "/fmp4/entry.m3u8")
    try:
        assert_vod(gateway, task, 5.5, video_frames=110, audio=True, fixtures=fixtures)
        check(u.count("/fmp4/master/child.m3u8") >= 1 and u.count("/fmp4/media/init.mp4") >= 1,
              "did not traverse both effective manifest URLs")
        check(u.count("/fmp4/master/init.mp4") == 0 and u.count("/fmp4/child.m3u8") == 0,
              "resolved a URL using the pre-redirect base")
    finally:
        gateway.delete(task)


def hls_external_ts_audio(gateway, fixtures):
    u = fixtures.upstream
    audio = fixtures.directory / "aux-audio.ts"
    run_media_tool("ffmpeg", [
        "-nostdin", "-y", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
        "-t", "4", "-c:a", "aac", "-b:a", "64k", "-f", "mpegts", str(audio),
    ])
    u.put("/aux-audio/video.m3u8", media_playlist(["video.ts"], final=True))
    u.put("/aux-audio/audio.m3u8", media_playlist(["audio.ts"], final=True))
    u.put("/aux-audio/video.ts", fixtures.ts()[0])
    u.put("/aux-audio/audio.ts", audio.read_bytes())
    path = "/aux-audio/master.m3u8"
    u.put(path, '#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="program",NAME="Audio",DEFAULT=YES,AUTOSELECT=YES,URI="audio.m3u8"\n#EXT-X-STREAM-INF:BANDWIDTH=900000,CODECS="avc1.42E01E,mp4a.40.2",AUDIO="program"\nvideo.m3u8\n')
    url = u.origin + path
    info = json.loads(gateway.body("/api/parse", payload={"mpd": url}))
    check(bool(info["audio"]), "auxiliary TS audio missing from input metadata")
    code, _, body = gateway.create_response(url, info["video"][0]["id"], audio_rep_id=info["audio"][0]["id"])
    if code == 400:
        check(b"audio" in body.lower() and b"not supported" in body.lower(),
              f"auxiliary audio rejected for an unrelated reason: {body!r}")
        return
    check(200 <= code < 300, f"unexpected HLS create response with auxiliary audio: {code} {body!r}")
    task = json.loads(body)
    try:
        status = gateway.wait_state(task, "finished", "error")
        if status["state"] == "error":
            error = status.get("error", "").lower()
            check("audio" in error and "not supported" in error,
                  f"auxiliary audio failed for an unrelated reason: {status!r}")
        else:
            assert_vod(gateway, task, 4.0, video_frames=80, audio=True, fixtures=fixtures)
    finally:
        gateway.delete(task)


def hls_live_recovery(gateway, fixtures):
    s0, s1, s2, _ = fixtures.ts()
    u = fixtures.upstream
    for name, body in [("a.ts", s0), ("bad.ts", s1[:-1]), ("b.ts", s1), ("c.ts", s2)]:
        u.put("/recovery/" + name, body)
    path = "/recovery/index.m3u8"
    u.put(path, media_playlist(["a.ts?sig=first"]))
    task = gateway.task(u.origin + path)
    try:
        eventually(lambda: gateway.status(task)["segments_done"] >= 1, "initial withheld video")
        segments = ["a.ts?sig=second", "bad.ts", "b.ts", "c.ts"]
        u.put(path, media_playlist(segments))
        def recovered():
            status = gateway.status(task)
            check(status["state"] != "error", f"truncated TS made live worker fatal: {status!r}")
            return status["segments_done"] >= 3
        eventually(recovered, "playback beyond truncated TS")
        check(u.count("/recovery/bad.ts") >= 2, "malformed clear TS was not retried")
        check(u.count("/recovery/a.ts?sig=second") == 0, "already-processed media sequence fetched after signature change")
        check(gateway.status(task)["segments_done"] == 3, "duplicate live video output")
        u.put(path, media_playlist(segments, final=True))
        gateway.finish(task)
        playlist = gateway.playlist(task)
        check("#EXT-X-ENDLIST" in playlist and len(playlist_times(playlist)) == 3,
              f"withheld live tail was not released at natural EOF: {playlist!r}")
    finally:
        gateway.delete(task)


def hls_subtitle_map(gateway, fixtures):
    ts = fixtures.ts()
    u = fixtures.upstream
    u.put("/subtitle/master.m3u8", '#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="s",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,URI="text.m3u8"\n#EXT-X-STREAM-INF:BANDWIDTH=400000,SUBTITLES="s"\nvideo.m3u8\n')
    u.put("/subtitle/video.m3u8", media_playlist(["a.ts", "b.ts"], final=True))
    u.put("/subtitle/text.m3u8", media_playlist(["a.vtt", "b.vtt"], final=True))
    starts = []
    for n, name in enumerate(("a", "b")):
        data = probe(fixtures.ts_paths[n], "-select_streams", "v:0", "-read_intervals", "%+#1",
                     "-show_entries", "packet=pts")
        pts = data["packets"][0]["pts"]
        starts.append(pts)
        u.put(f"/subtitle/{name}.ts", ts[n])
        u.put(f"/subtitle/{name}.vtt", f"WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:{pts}\n\n00:00:01.000 --> 00:00:02.000\ncaption {name}\n")
    task = gateway.task(u.origin + "/subtitle/master.m3u8", enable_subtitles=True)
    try:
        gateway.finish(task)
        playlist = gateway.playlist(task, "/xyz")
        urls = playlist_urls(playlist)
        check(len(urls) == 2, f"subtitle playlist should have two segments: {playlist!r}")
        for n, (pts, url, letter) in enumerate(zip(starts, urls, ("a", "b"))):
            text = gateway.body(url, auth=False).decode()
            check(f"X-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:{pts}" in text,
                  f"subtitle segment {n} lost its TS time map: {text!r}")
            check(f"00:00:01.000 --> 00:00:02.000\ncaption {letter}" in text,
                  f"subtitle segment {n} lost/moved its cue: {text!r}")
    finally:
        gateway.delete(task)


def hls_on_demand_window(gateway, fixtures):
    u = fixtures.upstream
    for n, data in enumerate(fixtures.ts()):
        u.put(f"/ondemand/s{n}.ts", data)
    path = "/ondemand/index.m3u8"
    u.put(path, media_playlist(["s0.ts"]))
    race_path = "/ondemand/race.m3u8"
    u.put(race_path, media_playlist(["race.ts"]))
    u.put("/ondemand/race.ts", fixtures.ts()[0])
    race_task = gateway.task(u.origin + race_path, run_mode="on_demand", window=1)
    try:
        gateway.wait_state(race_task, "paused")
        # Include several simultaneous manual starts. One can acquire the worker,
        # and the rest are rejected while the live worker remains active.
        start = f"/api/tasks/{race_task['id']}/start"
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as threads:
            outcomes = list(threads.map(lambda _: gateway.request(start, method="POST")[0], range(8)))
        check(outcomes.count(200) == 1 and outcomes.count(409) == 7,
              f"concurrent starts acquired more than one worker: {outcomes!r}")
    finally:
        gateway.delete(race_task)
    task = gateway.task(u.origin + path, run_mode="on_demand", window=1)
    try:
        gateway.wait_state(task, "paused")
        status, headers, body = gateway.request(task["playlist_url"], auth=False)
        check(status == 503 and headers.get("Retry-After"),
              f"withheld first segment produced a success/empty playlist response: {status} {body!r}")
        check(gateway.status(task)["segments_done"] == 1, "first playback did not wake the on-demand task")
        for count in (2, 3, 4):
            names = [f"s{n}.ts" for n in range(count)]
            u.put(path, media_playlist(names, discontinuities=[1]))
            eventually(lambda: gateway.status(task)["segments_done"] >= count,
                       f"{count} on-demand segments queued")
            playlist = gateway.playlist(task)
            check(len(playlist_times(playlist)) == 1, f"live window exposed withheld tail: {playlist!r}")
            if count == 3:
                check("#EXT-X-DISCONTINUITY\n" in playlist and "#EXT-X-DISCONTINUITY-SEQUENCE:0" in playlist,
                      f"discontinuity not visible before boundary slides: {playlist!r}")
            if count == 4:
                check("#EXT-X-DISCONTINUITY\n" not in playlist and "#EXT-X-DISCONTINUITY-SEQUENCE:1" in playlist,
                      f"discontinuity sequence did not advance when boundary slid: {playlist!r}")
        check(u.count("/ondemand/s0.ts") == 1, "first on-demand media downloaded repeatedly")
        u.put(path, media_playlist([f"s{n}.ts" for n in range(4)], final=True, discontinuities=[1]))
        gateway.finish(task)
        playlist = gateway.playlist(task)
        check(len(playlist_times(playlist)) == 2 and "#EXT-X-ENDLIST" in playlist,
              f"natural EOF failed to expose hidden live tail with endlist: {playlist!r}")
    finally:
        gateway.delete(task)


def startup_auth(gateway, _):
    data = json.loads(gateway.config.read_text())
    check(data["server"]["host"] == HOST, "generated config is not loopback by default")
    check(isinstance(data["auth"]["key"], str) and len(data["auth"]["key"]) >= 24,
          "generated config did not get a fresh management key")
    if os.name == "posix":
        check(stat.S_IMODE(gateway.config.stat().st_mode) & 0o077 == 0, "generated config is group/world-accessible")
    check(gateway.request("/api/tasks", auth=False)[0] == 401, "unauthenticated API call was accepted")
    check(gateway.request("/api/tasks")[0] == 200, "generated management key cannot authenticate")
    placeholder = gateway.directory / "placeholder.json"
    placeholder.write_text(json.dumps({"auth": {"key": "change-me"}}))
    result = subprocess.run(
        [str(gateway.binary), "--config", str(placeholder), "--host", HOST, "--port", "0"],
        env=gateway.env, capture_output=True, text=True, timeout=3, check=False,
    )
    check(result.returncode != 0 and "auth.key" in result.stderr + result.stdout,
          "gateway with a documented placeholder key did not refuse startup")


def worker_stop_resume(gateway, fixtures):
    u = fixtures.upstream
    gate = threading.Event()
    u.put("/worker/slow.ts", fixtures.ts()[0], gate=gate)
    path = "/worker/index.m3u8"
    u.put(path, media_playlist(["slow.ts"]))
    task = gateway.task(u.origin + path)
    try:
        eventually(lambda: u.count("/worker/slow.ts") == 1, "initial worker download waiting on upstream")
        control = f"/api/tasks/{task['id']}"
        code, _, body = gateway.request(control + "/stop", method="POST")
        check(code in (200, 202), f"worker stop was not accepted: {code} {body!r}")
        stopped = eventually(
            lambda: gateway.status(task)["state"] == "stopped",
            "worker termination before upstream unblocks", timeout=4,
        )
        check(stopped and gateway.status(task)["segments_done"] == 0, "cancelled worker published a segment")
        u.put("/worker/slow.ts", fixtures.ts()[0])
        gate.set()
        code, _, body = gateway.request(control + "/start", method="POST")
        check(200 <= code < 300, f"completed worker could not be restarted: {code} {body!r}")
        eventually(lambda: gateway.status(task)["segments_done"] == 1, "new worker output")
        u.put(path, media_playlist(["slow.ts"], final=True))
        gateway.finish(task)
        result = gateway.playlist(task)
        check(len(playlist_times(result)) == 1 and "#EXT-X-ENDLIST" in result,
              f"restart resulted in missing/duplicate output: {result!r}")
    finally:
        gate.set()
        gateway.delete(task)


def vod_budget(gateway, fixtures):
    u = fixtures.upstream
    packets = b"\x47\x1f\xff\x10" + bytes(184)  # legal MPEG-TS null packet
    for n, content in enumerate(fixtures.ts()[:2]):
        padded = content + packets * ((680 * 1024 - len(content)) // len(packets))
        u.put(f"/budget/s{n}.ts", padded)
    path = "/budget/index.m3u8"
    u.put(path, media_playlist(["s0.ts", "s1.ts"], final=True))
    task = gateway.task(u.origin + path)
    try:
        status = gateway.wait_state(task, "finished", "error")
        check(status["state"] == "error" and "output cache" in status.get("error", ""),
              f"VOD output exceeded byte budget without a cache error: {status!r}")
        code, _, body = gateway.request(task["playlist_url"], auth=False)
        check(code == 503 or (code == 200 and b"#EXT-X-ENDLIST" not in body),
              f"VOD budget error exposed a fake complete playlist: {code} {body!r}")
    finally:
        gateway.delete(task)


def global_budget(gateway, fixtures):
    u = fixtures.upstream
    content = fixtures.ts()[0]
    packet = b"\x47\x1f\xff\x10" + bytes(184)
    u.put("/global/segment.ts", content + packet * ((680 * 1024 - len(content)) // len(packet)))
    path = "/global/index.m3u8"
    u.put(path, media_playlist(["segment.ts"], final=True))
    url = u.origin + path
    first, second, retry = None, None, None
    try:
        first = gateway.task(url)
        gateway.finish(first)
        second = gateway.task(url)
        status = gateway.wait_state(second, "finished", "error")
        check(status["state"] == "error" and "output cache global limit" in status.get("error", ""),
              f"second task occupied more than the process-wide cache allowance: {status!r}")
        gateway.delete(first)
        first = None
        retry = gateway.task(url)
        gateway.finish(retry)
    finally:
        for task in (first, second, retry):
            if task:
                gateway.delete(task)


CASES = {
    "startup-auth": startup_auth,
    "dash-short-tail": dash_short_tail,
    "dash-track-isolation": dash_track_isolation,
    "dash-multiple-moof": dash_multiple_moof,
    "dash-period-extent": dash_period_extent,
    "dash-aligned-crop": dash_aligned_crop,
    "dash-live-endlist": dash_live_endlist,
    "hls-fmp4-redirect": hls_fmp4_redirect,
    "hls-external-ts-audio": hls_external_ts_audio,
    "hls-live-recovery": hls_live_recovery,
    "hls-subtitle-map": hls_subtitle_map,
    "hls-on-demand-window": hls_on_demand_window,
    "worker-stop-resume": worker_stop_resume,
    "vod-budget": vod_budget,
    "global-budget": global_budget,
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/dv-hls-gateway")
    parser.add_argument("--case", action="append", choices=CASES, help="run only this case (repeatable)")
    parser.add_argument("--list", action="store_true", help="list case names")
    args = parser.parse_args()
    if args.list:
        print("\n".join(CASES))
        return 0
    for program in ("ffmpeg", "ffprobe"):
        if not shutil.which(program):
            parser.error(f"{program} is required")
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"build the gateway first: {binary} does not exist")
    cases = list(dict.fromkeys(args.case or CASES))
    failures = 0
    with tempfile.TemporaryDirectory(prefix="dv-hls-gateway-smoke-") as work, Upstream() as upstream:
        root = Path(work)
        fixtures = Fixtures(root, upstream)
        with contextlib.ExitStack() as stack:
            gateway = None
            for name in cases:
                active_gateway = None
                try:
                    if name == "vod-budget":
                        active_gateway = stack.enter_context(Gateway(binary, root / "budget-gateway", DVHLS_OUTPUT_TASK_MIB="1"))
                    elif name == "global-budget":
                        active_gateway = stack.enter_context(Gateway(binary, root / "global-gateway", DVHLS_OUTPUT_GLOBAL_MIB="1"))
                    else:
                        if gateway is None:
                            gateway = stack.enter_context(Gateway(binary, root / "gateway"))
                        active_gateway = gateway
                    CASES[name](active_gateway, fixtures)
                    print(f"PASS {name}", flush=True)
                except (AssertionError, OSError, ValueError, subprocess.SubprocessError) as e:
                    failures += 1
                    print(f"FAIL {name}: {e}", flush=True)
                    if isinstance(e, subprocess.CalledProcessError) and e.stderr:
                        print(e.stderr.decode(errors="replace"), flush=True)
                    if active_gateway:
                        print(active_gateway.log_tail(), flush=True)
    print(f"{len(cases)-failures}/{len(cases)} loopback cases passed", flush=True)
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())
