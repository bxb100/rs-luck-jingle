# 叮当小印 D1X 打印服务

本项目将 GitHub Webhook 渲染为 384 点宽位图，并通过 Bluetooth Classic RFCOMM/SPP 发送到 `LuckP_D1X_` 打印机。

连接与打印协议以叮当小印 Android `2.7.19` 的实际 D1X 路径为依据。旧版 Rust 的 BLE UUID、分包、图像首部和初始化序列均已废弃，不能作为协议参考。完整协议见 [spec/d1x-classic-protocol.md](spec/d1x-classic-protocol.md)，机器可读向量见 [spec/fixtures/d1x-classic-vectors.json](spec/fixtures/d1x-classic-vectors.json)。

## 运行要求

- Rust `1.88` 或更高版本；容器构建固定使用 Rust `1.97.1` 和 Debian Bookworm。
- Linux 需要可用的 Bluetooth Classic 适配器和 BlueZ；macOS 使用系统自带的 IOBluetooth framework，并需要开启蓝牙。
- Linux 通过 BlueZ、macOS 通过 IOBluetooth 自动执行 BR/EDR 发现、SPP 服务查询和 RFCOMM 连接。两种平台都不要求先运行 `bluetoothctl`，也不要求预先配对或信任设备。

默认扫描名称以 `LuckP_D1X_` 开头的打印机。只有一个候选时自动选择；存在多个候选时会显示每台设备的名称、MAC 地址和可用的 RSSI，并要求输入序号。Linux 由 BlueZ、macOS 由 IOBluetooth 根据 SPP UUID 自动解析 RFCOMM channel；macOS inquiry 不提供可靠 RSSI 时会显示 `unknown`。

macOS discovery 在父进程主线程上运行 IOBluetooth RunLoop，并在配置的扫描时限到达后显式停止 inquiry；选中设备后，由同一可执行文件启动的内部 helper 子进程在其主线程持有 SDP/RFCOMM 对象，父进程只通过有界 IPC 帧传递读写请求。这避免后台线程收不到系统回调，也不依赖某些系统版本不会发送的 started 或 SDP completion 回调。inquiry 停止后仍未取得名称的设备会在受限预算内补做远程名称查询。失败时会区分“没有发现 Classic 设备”“设备名称无法解析”和“名称不匹配前缀”，并保留 MAC 地址用于诊断。

macOS 构建会把包含 `NSBluetoothAlwaysUsageDescription` 的用途说明嵌入两个命令行程序。首次运行时，仍需在“系统设置 → 隐私与安全性 → 蓝牙”中允许负责启动程序的宿主应用访问蓝牙，例如 Terminal、RustRover 或 Codex；发布独立程序时还应使用稳定代码签名。

## 配置

| 环境变量 | 必填 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `LUCK_JINGLE_PRINTER_ADDRESS` | 否 | 自动发现 | 无交互环境可用的打印机地址覆盖值 |
| `LUCK_JINGLE_RFCOMM_CHANNEL` | 否 | 自动 SDP | 仅用于诊断的 channel 覆盖值，范围 `1..30` |
| `LUCK_JINGLE_DISCOVERY_TIMEOUT_SECS` | 否 | `12` | BR/EDR 主动扫描时间，必须大于 `0`；macOS 名称补查另有受限预算 |
| `LUCK_JINGLE_DENSITY` | 否 | `1` | 打印浓度：`0`、`1` 或 `2`；启动时读取，不写入图片算法 |
| `LUCK_JINGLE_FEED_DOTS` | 否 | `80` | 每个普通卷纸作业结束后的走纸点数 |
| `LUCK_JINGLE_BIND_ADDRESS` | 否 | `0.0.0.0:5444` | HTTP 监听地址 |
| `LUCK_JINGLE_GITHUB_TOKEN` | 否 | 无 | 访问需要认证的 GitHub 附件；只发送到 GitHub 附件入口，跨域跳转时会移除 |
| `RUST_LOG` | 否 | `info` | Rust 日志过滤器；日志不会输出打印正文 |

```sh
export LUCK_JINGLE_DENSITY="1"
cargo run --release
```

需要更深的热敏输出时，通过环境变量选择档位，例如：

```sh
LUCK_JINGLE_DENSITY="2" cargo run --release
```

## 直接图片测试

在 Linux 或 macOS 主机上可以绕过 Webhook，直接使用同一套发现、连接和打印 session 输出图片。Linux 使用 BlueZ，macOS 使用 IOBluetooth；未提供路径时默认使用 `res/test_image.png`。

```sh
cargo run --release --bin print_image -- res/test_image.png
```

图片会按 Android 2.7.19 的默认图片模式处理：先使用等价于 Android Canvas 的单精度矩阵、双线性过滤和边缘抗锯齿，将图片绘制到 384 点宽的白色 RGB565 画布。RGB565 采用 Skia 的就近量化，OpenCV 再将其展开为低位补零的 RGBA 数据；Android 随后误用 `COLOR_BGRA2GRAY`，Rust 复现相同的通道顺序和灰度系数。灰度图再按全图均值应用自适应 gamma 和两行误差扩散，其中下一行中心像素在左右两列边缘或倒数第二行使用权重 `5`，其余位置使用权重 `3`，最后编码为 D1X 的单色光栅。该处理会保留亮图的中间调，避免固定阈值造成纸面过淡。打印浓度仍只由 `LUCK_JINGLE_DENSITY` 决定。单台打印机自动选择，多台打印机沿用名称、MAC 地址和 RSSI 选择流程。

例如，以最高受支持浓度打印一张原始照片：

```sh
LUCK_JINGLE_DENSITY="2" cargo run --release --bin print_image -- res/fox.png
```

## HTTP 接口

服务提供：

- `GET /`：存活检查。
- `POST /print`：接收并打印原始 Markdown 文本。
- `POST /github-webhooks`：接收 GitHub `issues`、`issue_comment` 和 `ping` 事件。

服务默认监听 `0.0.0.0:5444`，且不提供入站认证。只应在可信网络中开放；也可以将 `LUCK_JINGLE_BIND_ADDRESS` 改为 `127.0.0.1:5444`，或将服务置于带认证的反向代理之后。`LUCK_JINGLE_GITHUB_TOKEN` 只用于下载受保护的 GitHub 图片，不是 HTTP 接口的认证凭据。

### Markdown 打印

`POST /print` 的请求体是原始 UTF-8 Markdown，不使用 JSON 封装。`Content-Type` 必须是 `text/markdown`；可以省略 charset，若提供则只能是 `utf-8`。请求体上限为 64 KiB，按原始字节数计算。例如：

```sh
curl --fail-with-body \
  --include \
  --request POST \
  --header 'Content-Type: text/markdown; charset=utf-8' \
  --data-binary @document.md \
  http://127.0.0.1:5444/print
```

`Idempotency-Key` 可省略；提供时必须是 1 至 128 个可见 ASCII 字符。相同 key 在缓存中尚未淘汰时只会入队一次，重复请求仍返回 `202 Accepted`，并返回首次作业的 `X-Request-ID`。修改 Markdown 后测试时应省略或更换 key，否则不会创建新作业。`/print` key 与 GitHub delivery 使用不同命名空间，但共用一个最多 1024 项的缓存，因此混合请求会共同消耗容量。缓存只存在于当前进程，进程重启后不会保留。如果队列已满或已关闭，key 不会写入缓存，可以使用同一个 key 重试。

`/print` 正文不会应用 GitHub Webhook 通知的 60 字截断。标准 `![alt](https://...)` 和 HTML `<img src="https://...">` 图片都会处理。图片必须来自使用默认 443 端口的公网 HTTPS 地址；初始地址和每次重定向都会重新校验，DNS 结果中的环回、私网、链路本地、CGNAT、组播和保留地址会在连接前移除。为兼容透明代理的 fake-IP DNS，域名解析得到的 `198.18.0.0/15` 合成地址可以用于连接，但直接使用该网段 IP 的图片 URL 仍会被拒绝。相对路径、`file:`、`data:` 和 HTTP 图片无法从远程请求中取得，因此会打印占位提示。

每个请求最多处理 4 张图片，每张下载数据最多 10 MiB。超额、下载失败或无法解码的图片会替换为纸面占位提示。当前排版先打印全部文字，再按 Markdown 中的出现顺序将图片排列在文字之后，不支持图片与段落内联混排。

响应状态如下：

| 状态码 | 含义 |
| --- | --- |
| `202 Accepted` | 作业已进入内存队列，或相同 `Idempotency-Key` 的作业此前已入队；不表示已经连接打印机、完成打印或收到 D1X 停止确认 |
| `400 Bad Request` | 请求体不是有效 UTF-8、没有可打印内容，或 `Idempotency-Key` 无效 |
| `413 Payload Too Large` | 请求体超过 64 KiB |
| `415 Unsupported Media Type` | `Content-Type` 不是受支持的 Markdown UTF-8 类型 |
| `503 Service Unavailable` | 内存队列已满或已关闭；请求未被接收，可以按 `Retry-After` 重试 |

### GitHub Webhook

Issue 和评论正文中的公网 HTTPS Markdown 图片会被下载、缩放到 384 点宽，并与通知文字合成为一个打印作业。普通 Markdown 链接保留标签文字，不打印 URL。每个正文最多处理 4 张图片，每张下载数据最多 10 MiB；超额、下载失败或无法解码的图片会改为纸面占位提示，通知正文仍会打印。`LUCK_JINGLE_GITHUB_TOKEN` 只会发送到初始 GitHub attachment 入口，不会携带到重定向地址或普通图片域名。

Webhook 成功进入内存队列后返回 `202 Accepted`。打印由单一 worker 串行执行；只有收到 D1X 的停止确认后才记录成功。发生 partial write、超时或断线时会关闭 dirty session，且不会自动重放光栅数据，以免重复出纸。

同一 `X-GitHub-Delivery` 在共享去重缓存中尚未淘汰时只入队一次。队列和最多 1024 项的共享去重缓存都只存在于当前进程；进程退出时尚未打印的任务不会保留，重启后也不能依赖该缓存证明幂等。若日志报告打印结果未知，必须人工确认纸面结果，不得通过新的 delivery 自动重放。

## 验证

```sh
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

真实打印验证可以在 Linux 或 macOS 宿主机执行。验证时应先清除目标设备已有的配对和信任状态，确认程序仍能完成主动发现、候选选择、自动 SDP 和 RFCOMM 连接；macOS 还应确认启动程序已获蓝牙权限。结果未知的作业不得自动重试。

## Docker

容器需要访问宿主机 Bluetooth socket。具体权限取决于部署环境；通常应在宿主机完成配对，并为容器提供 host network 与最小必要的网络能力。

```sh
docker build --target test -t rs-luck-jingle:test .
docker build -t rs-luck-jingle .
docker run --rm \
  --network host \
  --cap-add NET_RAW \
  -e LUCK_JINGLE_PRINTER_ADDRESS=AA:BB:CC:DD:EE:FF \
  rs-luck-jingle
```

若宿主机策略仍拒绝 RFCOMM socket，应在确认风险后按部署平台补充最小权限，不要直接使用全特权容器。
