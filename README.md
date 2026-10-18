# rscrcpy

用 Rust 写的 [scrcpy](https://github.com/Genymobile/scrcpy) 客户端：在电脑上显示并控制 Android 手机。
手机端直接使用官方 scrcpy-server（v5.0.1），本项目实现的是电脑端：adb 管理、协议解析、H.264 解码、输入控制和图形界面。

## 功能

- **图形界面**（egui）：设备列表自动刷新、无线配对/连接、投屏参数（分辨率上限、码率、帧率、Baseline、保持唤醒、剪贴板同步）
- **投屏**：画面自适应窗口，旋转后自动调整；状态栏显示 fps / 码率 / 解码错误
- **控制**：点击、滑动、Ctrl+拖动双指缩放、滚轮滚动；键盘输入，中文等非 ASCII 文本经剪贴板粘贴（支持输入法）
- **工具栏**：返回、主页、多任务、音量、电源、旋转、通知栏、快捷设置、息屏投屏、截图、复制
- **剪贴板**：Ctrl+V 把电脑剪贴板粘贴到手机，Ctrl+C / 复制按钮取回手机选中的内容，手机剪贴板变化自动同步到电脑
- **文件**：拖入 APK 直接安装，拖入其他文件传到 `/sdcard/Download/`
- **稳定性**：连接超时、端口占用时自动换端口、断线检测、只解码最新帧（不积压延迟）、退出时清理 adb forward 和手机端进程

## 项目结构

```
crates/
  rscrcpy-core/   协议核心库：adb、启动 server、视频解码、控制消息、会话管理（无 GUI 依赖）
  rscrcpy-gui/    桌面图形界面（eframe/egui），可执行文件名 rscrcpy
  rscrcpy-cli/    极简 minifb 窗口，用于调试 core
mock/             假 adb + 模拟 scrcpy server，无手机时测试用
scrcpy-server     官方 scrcpy-server v5.0.1（未修改，见 NOTICE）
```

## 构建

需要：
- Rust 1.95 或更新版本
- C 编译器（OpenH264 解码器从源码编译；Windows 用 MSVC Build Tools，macOS 用 Xcode Command Line Tools）
- 运行时需要 `adb`：放在 PATH 中、程序旁边、`platform-tools/` 目录，或设置 `ANDROID_HOME`；也可以用 `ADB` 环境变量或 `--adb` 指定

```sh
cargo build --release          # 生成 target/release/rscrcpy（GUI）
cargo run --release            # 直接运行 GUI
cargo run --release -p rscrcpy-cli -- -s SERIAL   # 调试用的极简窗口
```

分发时把 `scrcpy-server` 放在可执行文件旁边。如需重新下载并校验：`scripts/fetch-server.sh`。

## 使用

手机打开「开发者选项 → USB 调试」，连上电脑后运行 `rscrcpy`，在左侧选择设备点击「开始投屏」。

无线连接（Android 11+）：手机「开发者选项 → 无线调试 → 使用配对码配对」，在「无线连接」里填配对地址和配对码，配对后再填连接地址。

命令行参数：

```
rscrcpy [-s SERIAL] [--server PATH] [--adb PATH] [-m MAX_SIZE] [-b BIT_RATE] [--max-fps FPS]
```

带 `-s` 时启动后自动连接该设备。

### 操作

| 操作 | 作用 |
| --- | --- |
| 左键 | 点击 / 滑动 |
| Ctrl + 左键拖动 | 双指缩放 |
| 右键 / Esc | 返回 |
| 中键 / F1 | 主页 |
| F2 | 多任务 |
| F3 / F4 | 音量 - / + |
| F5 | 电源 |
| 滚轮 | 滚动 |
| 键盘 | 输入文字；Enter、Backspace、方向键、Tab、Ctrl+A 等按原样发送 |
| Ctrl+V / Ctrl+C | 粘贴到手机 / 复制手机选中内容 |

截图保存在 `~/Pictures/rscrcpy/`（没有 Pictures 目录时保存在当前目录的 `screenshots/`）。
界面会自动寻找系统中文字体；找不到时可以设置 `RSCRCPY_FONT=字体文件路径`。

## 无手机测试

`mock/` 下的假 adb 会启动一个用 Python 写的模拟 server：循环播放 `mock/test.h264`，
并把收到的控制消息逐条写入 `mock/control.log`（Linux / macOS，需要 python3）。

```sh
cargo test --workspace                          # 包含基于 mock 的端到端测试
ADB=mock/bin/adb cargo run -- -s mock-0001      # 手动试用界面，另开终端 tail -f mock/control.log
```

## 已知限制

- 只支持 H.264 视频，软件解码（OpenH264）；暂不支持音频
- 中文输入依赖剪贴板粘贴，个别应用的输入框可能不接受
- 目前主要用 mock 测试，真机兼容性还需要更多验证

## 致谢

手机端使用 [scrcpy](https://github.com/Genymobile/scrcpy) 的 scrcpy-server（Apache License 2.0），详见 [NOTICE](NOTICE) 和 [licenses/scrcpy-LICENSE](licenses/scrcpy-LICENSE)。
