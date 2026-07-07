# DV-HLS Gateway

DV-HLS Gateway 是一个纯净的手动推流器，用于把 DASH(MPD) 或 HLS(M3U8) 输入实时转封装为明文 HLS-TS 输出。

它不依赖 ffmpeg / mp4decrypt，运行时只做解密、MP4 解析、TS 封装和直播窗口发布；HEVC、Dolby Vision RPU、HDR 元数据、AAC / AC-3 / EC-3 音频都会尽量原样透传。

## 功能

- 支持 MPD 和 M3U8 输入，统一输出 HLS-TS。
- 支持 VOD 和 Live，Live 会贴近 live edge、滚动发布并做内存 GC。
- 支持 HEVC、H.264、Dolby Vision Profile 5/8、HDR10、HLG、SDR。
- 支持 AAC-LC、AC-3、EC-3，前端可显示语言、codec 和码率。
- 支持 DASH CENC AES-CTR、HLS AES-128、HLS fMP4 SAMPLE-AES / cbcs。
- 支持固定 key 和动态取 key。
- 支持字幕推流，开启字幕时输出 master playlist + media playlist + subtitle playlist。
- 支持持续转化和按需启停，按需任务默认 5 分钟无播放请求后暂停。
- 分片只缓存在内存中，不写磁盘。
- 输出 TS 分片伪装为 `.jpeg` 且响应 `Content-Type: image/jpeg`，便于 CDN 缓存。

## 目录

- `src/`：主程序源码。
- `src/frontend/index.html`：内置 Web UI。
- `examples/`：辅助示例。
- `examples/key_api.php`：动态取 key 接口 PHP 示例。
- `dv-hls-gateway.example.json`：运行配置示例。
- `.github/workflows/ci.yml`：CI，运行格式检查和测试。
- `.github/workflows/release.yml`：多平台二进制构建。

## 构建

### 本地构建

```bash
cargo build --release
```

运行：

```bash
./target/release/dv-hls-gateway
```

默认读取二进制同目录的 `dv-hls-gateway.json`。如果文件不存在，程序首次启动会自动生成一个模板。

也可以显式指定配置文件和端口：

```bash
./target/release/dv-hls-gateway --config ./dv-hls-gateway.json --host 0.0.0.0 --port 37201
```

### GitHub Actions 构建

仓库内置 `Build Release Binaries` workflow，可手动运行，也可推送 `v*` tag 自动构建并发布附件。

手动运行：

1. 打开 GitHub Actions。
2. 选择 `Build Release Binaries`。
3. 点击 `Run workflow`。
4. 填入 `release_tag`，例如 `v0.1.0`。

手动运行会创建或更新对应 tag 的 Release。推送 tag 也会自动发布：

固定产物名：

| 产物 | 运行平台 | Rust target |
|---|---|---|
| `dv-hls-gateway-linux-amd64-musl` | Linux x86_64 | `x86_64-unknown-linux-musl` |
| `dv-hls-gateway-linux-armv7-musl` | Linux ARMv7 hard-float | `armv7-unknown-linux-musleabihf` |
| `dv-hls-gateway-linux-arm64-musl` | Linux ARM64 | `aarch64-unknown-linux-musl` |
| `dv-hls-gateway-windows-amd64-musl.exe` | Windows x86_64 | `x86_64-pc-windows-msvc` |
| `dv-hls-gateway-macos-amd64-musl` | macOS Intel | `x86_64-apple-darwin` |
| `dv-hls-gateway-macos-arm64-musl` | macOS Apple Silicon | `aarch64-apple-darwin` |

说明：`musl` 是 Linux 目标使用的 libc；Windows/macOS 没有 musl ABI，项目仍按固定产物名输出，实际 target 以表格为准。

命令行打 tag 触发构建：

```bash
git tag v0.1.0
git push origin v0.1.0
```

下载后运行：

```bash
chmod +x ./dv-hls-gateway-linux-amd64-musl
./dv-hls-gateway-linux-amd64-musl --config ./dv-hls-gateway.json --host 0.0.0.0 --port 37201
```

Windows：

```powershell
.\dv-hls-gateway-windows-amd64-musl.exe --config .\dv-hls-gateway.json --host 0.0.0.0 --port 37201
```

macOS：

```bash
chmod +x ./dv-hls-gateway-macos-arm64-musl
./dv-hls-gateway-macos-arm64-musl --config ./dv-hls-gateway.json --host 0.0.0.0 --port 37201
```

## 配置

推荐先复制示例配置：

```bash
cp dv-hls-gateway.example.json dv-hls-gateway.json
```

示例：

```json
{
  "server": {
    "host": "0.0.0.0",
    "port": 37201
  },
  "auth": {
    "key": "change-me"
  },
  "key_api": {
    "url": "http://127.0.0.1:45689/keys",
    "token": "change-this-token",
    "attempts": 12,
    "retry_base_ms": 400,
    "retry_max_ms": 8000
  }
}
```

字段说明：

| 字段 | 说明 |
|---|---|
| `server.host` | HTTP 监听地址，常用 `0.0.0.0` |
| `server.port` | HTTP 监听端口 |
| `auth.key` | Web 面板和 `/api/*` 的访问密钥 |
| `key_api.url` | 动态取 key 接口 URL |
| `key_api.token` | 动态取 key 接口的 `X-Token` |
| `key_api.attempts` | 动态取 key 最大尝试次数 |
| `key_api.retry_base_ms` | 取 key 失败后的递增重试基础延迟 |
| `key_api.retry_max_ms` | 单次取 key 重试最大延迟 |

如果旧配置没有 `auth.key`，程序启动时会自动补一个随机密钥并写回配置文件。

## PM2

从配置文件读取端口：

```bash
pm2 start ./dv-hls-gateway-linux-amd64-musl --name dv-hls-gateway
```

临时覆盖端口：

```bash
pm2 start ./dv-hls-gateway-linux-amd64-musl --name dv-hls-gateway -- --port 37201
```

指定配置文件：

```bash
pm2 start ./dv-hls-gateway-linux-amd64-musl --name dv-hls-gateway -- --config ./dv-hls-gateway.json
```

## Web UI

1. 打开 `http://127.0.0.1:37201`。
2. 输入 `auth.key`。
3. 填入 MPD / M3U8 URL。
4. 选择 key 模式：
   - 固定 key：在 `KEYS` 输入框填一行或多行 `KID:KEY`。
   - 动态取 key：勾选“动态取 Key”，程序会解析 KID 并调用 `key_api.url`。
5. 点“解析轨道”。
6. 选择视频轨、音频轨，可选字幕轨。
7. 选择“持续转化”或“按需启停”。
8. 点“启动转封装”，复制 `/p/<task-id>` 播放地址。

没有勾选“推流字幕”时，`/p/<task-id>` 直接返回单层 media playlist。勾选字幕后，`/p/<task-id>` 返回 master playlist，音视频 playlist 伪装为 `/p/<task-id>/api`，字幕 playlist 伪装为 `/p/<task-id>/xyz`。

## 动态取 Key

动态取 key 用于 KID 会变化或输入源存在多 KID 的场景。程序会从 MPD / HLS manifest、init segment、加密信息中解析需要的 KID；本地 key store 中缺少某个 KID 时，会调用配置里的取 key 接口。

### 请求

程序会发起 HTTP POST：

```http
POST /keys HTTP/1.1
Content-Type: application/json
X-Token: <key_api.token>
```

请求体必须是 JSON object，字段 `kid` 是字符串数组：

```json
{
  "kid": [
    "00112233445566778899aabbccddeeff",
    "11223344556677889900aabbccddeeff"
  ]
}
```

KID 格式：

- 32 位十六进制字符串。
- 可以大小写混用，程序会归一化为小写。
- 接口实现建议同时兼容带连字符 UUID，内部去掉 `-` 后匹配。

### 响应

响应必须是 JSON 字符串数组，每个元素都是：

```text
KID:KEY
```

示例：

```json
[
  "00112233445566778899aabbccddeeff:ffeeddccbbaa99887766554433221100",
  "11223344556677889900aabbccddeeff:00112233445566778899aabbccddeeff"
]
```

响应要求：

- HTTP 状态码必须是 2xx。
- 响应体必须能解析为 JSON string array。
- 数组不能为空。
- 每个元素都必须包含 `:`。
- `KID` 必须是合法 16 字节十六进制。
- `KEY` 必须是合法 16 字节十六进制。
- 响应必须覆盖请求中的所有 KID。少一个 KID 都会被视为失败。

错误响应示例：

```json
{
  "error": "missing key",
  "missing": ["00112233445566778899aabbccddeeff"]
}
```

程序不会接受这种错误对象作为成功结果，因为成功结果必须是 JSON 字符串数组。

### 重试策略

如果取 key 接口出现以下情况，程序会认为本次取 key 失败并重试：

- 网络连接失败。
- 请求超时。
- HTTP 非 2xx。
- 响应不是 JSON 字符串数组。
- 数组为空。
- 某个元素不是 `KID:KEY`。
- KID / KEY 不是 16 字节十六进制。
- 返回结果没有覆盖全部请求 KID。

重试次数和延迟由配置控制：

```json
{
  "key_api": {
    "attempts": 12,
    "retry_base_ms": 400,
    "retry_max_ms": 8000
  }
}
```

延迟是递增的：第 1 次失败等待约 `retry_base_ms`，第 2 次失败等待约 `retry_base_ms * 2`，最高不超过 `retry_max_ms`。

也可以用环境变量临时覆盖：

| 变量 | 作用 |
|---|---|
| `DVHLS_KEY_API_URL` | 覆盖 `key_api.url` |
| `DVHLS_KEY_API_TOKEN` | 覆盖 `key_api.token` |
| `DVHLS_KEY_API_ATTEMPTS` | 覆盖最大尝试次数 |
| `DVHLS_KEY_API_RETRY_BASE_MS` | 覆盖基础延迟 |
| `DVHLS_KEY_API_RETRY_MAX_MS` | 覆盖最大延迟 |

### PHP 示例接口

示例脚本位于：

```text
examples/key_api.php
```

准备 key store：

```bash
cat > keys.json <<'JSON'
{
  "00112233445566778899aabbccddeeff": "ffeeddccbbaa99887766554433221100",
  "11223344556677889900aabbccddeeff": "00112233445566778899aabbccddeeff"
}
JSON
```

启动 PHP 内置服务器：

```bash
DVHLS_KEY_API_TOKEN='change-this-token' \
DVHLS_KEY_STORE='./keys.json' \
php -S 127.0.0.1:45689 examples/key_api.php
```

测试接口：

```bash
curl -X POST 'http://127.0.0.1:45689/keys' \
  -H 'content-type: application/json' \
  -H 'X-Token: change-this-token' \
  -d '{
    "kid": [
      "00112233445566778899aabbccddeeff",
      "11223344556677889900aabbccddeeff"
    ]
  }'
```

期望返回：

```json
[
  "00112233445566778899aabbccddeeff:ffeeddccbbaa99887766554433221100",
  "11223344556677889900aabbccddeeff:00112233445566778899aabbccddeeff"
]
```

然后在 `dv-hls-gateway.json` 中配置：

```json
{
  "key_api": {
    "url": "http://127.0.0.1:45689/keys",
    "token": "change-this-token",
    "attempts": 12,
    "retry_base_ms": 400,
    "retry_max_ms": 8000
  }
}
```

## API

所有 `/api/*` 都需要 `X-Auth-Key`：

```bash
K="X-Auth-Key: $(jq -r .auth.key ./dv-hls-gateway.json)"
```

解析轨道：

```bash
curl -X POST http://127.0.0.1:37201/api/parse \
  -H "$K" \
  -H 'content-type: application/json' \
  -d '{"mpd":"<MPD_OR_M3U8_URL>"}'
```

创建固定 key 任务：

```bash
curl -X POST http://127.0.0.1:37201/api/tasks \
  -H "$K" \
  -H 'content-type: application/json' \
  -d '{
    "name": "Live task",
    "mpd": "<MPD_OR_M3U8_URL>",
    "key_mode": "static",
    "keys": "00112233445566778899aabbccddeeff:ffeeddccbbaa99887766554433221100",
    "run_mode": "always",
    "video_rep_id": "<video-rep-id>",
    "audio_rep_id": "<audio-rep-id>",
    "enable_subtitles": false,
    "subtitle_rep_id": null,
    "window": 6,
    "target_duration": 7
  }'
```

创建动态 key 任务：

```bash
curl -X POST http://127.0.0.1:37201/api/tasks \
  -H "$K" \
  -H 'content-type: application/json' \
  -d '{
    "name": "Dynamic key live",
    "mpd": "<MPD_OR_M3U8_URL>",
    "key_mode": "dynamic",
    "keys": "",
    "run_mode": "always",
    "video_rep_id": "<video-rep-id>",
    "audio_rep_id": "<audio-rep-id>",
    "enable_subtitles": true,
    "subtitle_rep_id": "<subtitle-rep-id>",
    "window": 6,
    "target_duration": 7
  }'
```

任务控制：

```bash
curl -X POST -H "$K" http://127.0.0.1:37201/api/tasks/<task-id>/pause
curl -X POST -H "$K" http://127.0.0.1:37201/api/tasks/<task-id>/start
curl -X POST -H "$K" http://127.0.0.1:37201/api/tasks/<task-id>/stop
curl -X DELETE -H "$K" http://127.0.0.1:37201/api/tasks/<task-id>
```

播放地址：

```bash
ffplay "http://127.0.0.1:37201/p/<task-id>"
vlc "http://127.0.0.1:37201/p/<task-id>"
```

## 输出与缓存

默认播放入口：

```text
/p/<task-id>
```

未启用字幕：

```text
/p/<task-id>                    media playlist
/p/<task-id>/picture-<seq>.jpeg TS 分片
```

启用字幕：

```text
/p/<task-id>                    master playlist
/p/<task-id>/api                media playlist
/p/<task-id>/xyz                subtitle playlist
/p/<task-id>/xyz-<seq>.txt      WebVTT 字幕内容，txt 后缀与 text/plain 响应头
/p/<task-id>/picture-<seq>.jpeg TS 分片
```

Live 任务的内存保留上限约为：

```text
Live 窗口段数 + publish_delay 段数 + grace 段数
```

默认 `window=6`、`publish_delay=1`、`grace=3`，单任务默认最多保留约 `10` 个最终输出分片。VOD 任务使用 `window=0`，会保留全部已产段直到任务删除。

## CDN 缓存规则

分片长缓存：

```text
http.request.uri.path contains "/picture-" and ends_with(http.request.uri.path, ".jpeg")
```

字幕短缓存：

```text
http.request.uri.path contains "/xyz-" and ends_with(http.request.uri.path, ".txt")
```

分片与字幕合并缓存表达式：

```text
(http.request.uri.path contains "/picture-" and ends_with(http.request.uri.path, ".jpeg")) or (http.request.uri.path contains "/xyz-" and ends_with(http.request.uri.path, ".txt"))
```

playlist 微缓存或不缓存：

```text
starts_with(http.request.uri.path, "/p/") and not (http.request.uri.path contains "/picture-") and not (http.request.uri.path contains "/xyz-")
```

规则顺序建议：

1. `.jpeg` 分片长缓存。
2. `xyz-*.txt` 字幕短缓存。
3. playlist 微缓存或 bypass。

## 环境变量

| 变量 | 默认 | 作用 |
|---|---:|---|
| `MPD_HLS_PUBLISH_DELAY_SEGMENTS` | `1` | Live 输出隐藏最新 N 个已产段 |
| `MPD_HLS_SHORT_PUBLISH_DELAY_SEGMENTS` | `1` | 短源分片场景 publish delay |
| `MPD_HLS_SHARED_DOWNLOAD_CONCURRENCY` | `10` | 常规源共享下载并发 |
| `MPD_HLS_SHORT_SHARED_DOWNLOAD_CONCURRENCY` | `10` | 短分片源共享下载并发 |
| `MPD_HLS_SEGMENT_FETCH_CONCURRENCY` | `10` | 常规源分片抓取并发 |
| `MPD_HLS_SHORT_SEGMENT_FETCH_CONCURRENCY` | `10` | 短分片源分片抓取并发 |
| `MPD_HLS_ADAPTIVE_FETCH` | `1` | 是否启用任务级自适应并发 |
| `MPD_HLS_ON_DEMAND_IDLE_TIMEOUT_SECS` | `300` | 按需任务空闲暂停秒数 |
| `DVHLS_KEY_API_URL` | 配置文件 | 覆盖动态 key 接口 URL |
| `DVHLS_KEY_API_TOKEN` | 配置文件 | 覆盖动态 key 接口 token |
| `DVHLS_KEY_API_ATTEMPTS` | 配置文件 | 覆盖取 key 尝试次数 |
| `DVHLS_KEY_API_RETRY_BASE_MS` | 配置文件 | 覆盖取 key 基础重试延迟 |
| `DVHLS_KEY_API_RETRY_MAX_MS` | 配置文件 | 覆盖取 key 最大重试延迟 |

## 测试

```bash
cargo fmt -- --check
cargo test
```

可选 golden 测试支持外部样本：

```bash
DVHLS_GOLDEN_SEG_DIR='./sample-segments' \
DVHLS_GOLDEN_VIDEO_KEY='00112233445566778899aabbccddeeff:ffeeddccbbaa99887766554433221100' \
DVHLS_GOLDEN_AUDIO_KEY='11223344556677889900aabbccddeeff:00112233445566778899aabbccddeeff' \
cargo test --test cenc_golden
```

## 注意事项

- 任务和输出分片都在内存里，服务重启后旧 `/p/<task-id>` 会失效。
- 删除任务会释放该任务的内存分片队列。
- 浏览器通常不适合播放 HEVC / Dolby Vision，请使用 IINA、VLC、ffplay 或硬件播放器测试。
- 动态 key 模式下，取 key 接口必须稳定可达；如果接口长期不可用，任务会等待重试而不是发布无法解密的错误分片。
