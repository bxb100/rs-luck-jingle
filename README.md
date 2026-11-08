# rs-luck-jingle

基于 Rust 实现的叮当同学 D1 (`LuckP_D1X_`) 热敏打印机驱动与打印服务。通过 Bluetooth Classic (RFCOMM/SPP) 连接打印机，将 Markdown 文本、GitHub Webhook 或本地图片渲染为 384 点宽位图光栅并打印。

## 功能

- **蓝牙经典连接**：支持 Linux (BlueZ) 与 macOS (IOBluetooth)，自动发现并建立 RFCOMM 连接，支持心跳检测与断线重连。
- **Markdown 打印**：提供 HTTP 接口接收 Markdown 文本，自动排版并下载解析公网 HTTPS 图片。
- **GitHub Webhook**：接收 GitHub `issues`、`issue_comment` 和 `ping` 事件并自动打印通知。
- **直接图片打印**：提供 CLI 工具，复现官方自适应 gamma 与误差扩散抖动算法打印本地图像。

## 使用方法

### 运行服务

```sh
# 默认启动（自动扫描打印机，监听 0.0.0.0:5444）
cargo run --release

# 指定打印浓度（0、1、2）或设备地址
LUCK_JINGLE_DENSITY="2" LUCK_JINGLE_PRINTER_ADDRESS="AA:BB:CC:DD:EE:FF" cargo run --release
```

### 打印 Markdown

```sh
curl --request POST \
  --header 'Content-Type: text/markdown; charset=utf-8' \
  --data-binary @document.md \
  http://127.0.0.1:5444/print
```

### 打印本地图片

```sh
cargo run --release --bin print_image -- path/to/image.png
```

### GitHub Webhook

在 GitHub 仓库中添加 Webhook：
- Payload URL: `http://<HOST>:5444/github-webhooks`
- Content type: `application/json`
- Events: `Issues`, `Issue comments`

## Reference

- [LuckP D1X 蓝牙经典串口互操作协议](spec/d1x-classic-protocol.md)
- [D1X 协议测试向量](spec/fixtures/d1x-classic-vectors.json)
- 叮当小印 Android 应用（`com.dingdang.newprint` v2.7.19）
