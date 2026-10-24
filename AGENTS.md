# Agent 接手说明

本文件给在新机器上接手/运行 diffRact 的 agent 看。完整使用说明在 [README.md](README.md)，架构和算法在 [docs/design.md](docs/design.md)。

## 1. 先确认环境

- Windows 10/11 x64；
- Rust 1.92+；
- 可用的 MSVC C++ 构建环境；
- 运行测试：

  ```bat
  cargo test
  ```

## 2. 模型不在 git 里

`models/` 已被 `.gitignore` 忽略，克隆代码后不会自动带有语义模型。没有模型时程序仍可运行，但语义匹配会降级为纯文本，并在界面/报告中提示原因。

## 3. 获取模型

### 最快：使用内置命令

源码构建后直接运行：

```powershell
cargo run -- download-model --mirror
```

默认下载到 `models/`；指定目录用 `--dir`，覆盖已有文件用 `--force`。只有这个子命令访问网络。如果已有单文件 `model.onnx`（便携包结构），会自动跳过外置权重 `model.onnx_data`。

### 离线：从便携包复制

如果仓库里或手边有 `dist/diffRact-0.1.0-win64.zip`，直接取其中的 `models/`：

```powershell
Expand-Archive .\dist\diffRact-0.1.0-win64.zip -DestinationPath .\dist\portable
New-Item -ItemType Directory -Force .\models | Out-Null
Copy-Item .\dist\portable\diffRact-0.1.0-win64\models\* .\models\ -Force
```

### 从 Hugging Face 下载

```powershell
python -m pip install -U huggingface_hub

# 国内网络访问 huggingface.co 失败时使用镜像
$env:HF_ENDPOINT = "https://hf-mirror.com"

hf download onnx-community/bge-small-zh-v1.5-ONNX --local-dir models/bge-small-zh-v1.5-ONNX
```

然后把模型整理成默认结构：

```powershell
Copy-Item models\bge-small-zh-v1.5-ONNX\onnx\model.onnx .\models\model.onnx -Force
Copy-Item models\bge-small-zh-v1.5-ONNX\onnx\model.onnx_data .\models\model.onnx_data -Force
Copy-Item models\bge-small-zh-v1.5-ONNX\tokenizer.json .\models\tokenizer.json -Force
```

注意：

- 当前 ONNX Community 版本使用外部权重文件，`model.onnx` 和 `model.onnx_data` 必须放在同一目录；
- 如果下载到的是单文件 `model.onnx`（没有 `model.onnx_data`），只复制 `model.onnx` 和 `tokenizer.json`；
- 也可以不复制，直接把 `diffract.toml` 的 `path` / `tokenizer_path` 指向上面的下载目录；
- 模型文件很大，不要提交到 git。

最终默认结构：

```text
models/
  model.onnx
  model.onnx_data   # 使用外部权重时才需要
  tokenizer.json
```

## 4. 验证模型

```bat
cargo run -- --require-semantic --check 旧版.docx 新版.docx
```

看到“语义匹配已启用（model.onnx，512 维）”即正常。没有 docx 时，先跑 `cargo test` 验证代码；模型加载验证需要任取两份 `.docx`。

## 5. 构建与测试

```bat
cargo fmt --check
cargo clippy --all-targets
cargo test
cargo build --release
```

产物：`target/release/diffract.exe`。

## 6. 不要提交的东西

- `models/`
- `target/`
- `dist/`
- `WORKLOG.md`（如果根目录存在，这是本地工作记录，已在 `.git/info/exclude` 中）

## 7. 背景

表格列对齐 + 单元格差异已完成，提交 `1f70b47`；总体设计文档在 `docs/design.md`。TUI 切换记录卡顿已优化，提交 `09b3cad`。如果还有疑问，先读 README 的“启用语义匹配”和 `docs/design.md` 第 5.5 节。
