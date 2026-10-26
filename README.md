# diffRact

本地 .docx 版本比较工具。把两个版本的 Word 文档按段落配对，表格按表头对齐列、按行配对并定位到发生变化的单元格，输出六类结果：

| 状态 | 含义 |
| --- | --- |
| 未变（unchanged） | 归一化后文本完全相同，且位置未变 |
| 修改（modified） | 找到对应的段落或表格行，综合相似度达到确认阈值 |
| 疑似修改（suspected_modified） | 找到对应的段落或表格行，相似度介于疑似阈值与确认阈值之间，需人工确认 |
| 新增（added） | 新文档中没有对应段落或表格行 |
| 删除（removed） | 旧文档中的段落或表格行在新文档中没有对应项 |
| 移动（moved） | 归一化后文本完全相同，但位置发生变化 |

所有计算均在本机完成，不上传文档。正常比较不访问网络；只有显式执行 `download-model` 下载语义模型时会联网。

## 便携版

解压 `diffRact-0.2.0-win64.zip` 后目录内容：

```
diffRact-0.2.0-win64/
├─ diffract.exe             主程序（命令行名 diffract）
├─ MSVCP140.dll             VC++ 运行库（已随包提供）
├─ MSVCP140_1.dll
├─ VCRUNTIME140.dll
├─ VCRUNTIME140_1.dll
├─ diffract.toml            默认配置（启用内置语义模型）
├─ diffract.toml.example    配置模板
├─ models/
│  ├─ model.onnx            BAAI/bge-small-zh-v1.5（512 维）
│  └─ tokenizer.json
├─ docs/
│  └─ design.md             项目总体设计
└─ README.md
```

支持 Windows 10/11 x64，无需安装。删除或改名 `models/` 后，程序自动退化为纯文本匹配，并在界面和报告中明确提示。

### 快速开始

在程序目录打开终端：

```bat
diffract.exe 旧版.docx 新版.docx
```

不带参数启动会先进入文件选择器：先选旧版，再选新版；第二步从旧版所在目录开始。只给一个文件时，选择器只补选缺少的那个，同样从已给文件所在目录开始。`--check`、`--html`、`--md` 仍要求两个文件都给。

带路径或空格时加引号：

```bat
diffract.exe "D:\文档\方案 v1.docx" "D:\文档\方案 v2.docx"
```

CI / 批处理模式：

```bat
diffract.exe --check 旧版.docx 新版.docx
```

纯文本模式（不加载语义模型）：

```bat
diffract.exe --check --no-semantic 旧版.docx 新版.docx
```

导出报告（不进入 TUI）：

```bat
diffract.exe --html 对比报告.html --md 对比报告.md 旧版.docx 新版.docx
```

`--html` / `--md` 可单独使用或同时使用，会自动创建所在目录；指定任一导出参数即进入非交互模式，退出码与 `--check` 相同。也可以与 `--check` 一起用。

### 启用语义匹配

源码仓库不包含模型文件（`models/` 已列入 `.gitignore`），需要自行准备 `BAAI/bge-small-zh-v1.5` 的 ONNX 导出和 tokenizer。便携包是在构建时把模型一起打包的，所以解压即可用。

#### 方式 A：内置下载命令（推荐，会联网）

程序自带 `download-model` 子命令，默认从 Hugging Face 下载到当前目录的 `models/`：

```bat
diffract.exe download-model
```

国内网络建议使用镜像：

```bat
diffract.exe download-model --mirror
```

指定目录或覆盖已有文件：

```bat
diffract.exe download-model --dir D:\models --force
```

命令会下载并整理成默认配置需要的结构：

```text
models/
  model.onnx
  model.onnx_data   # 当前 ONNX 版本的外置权重
  tokenizer.json
```

`download-model` 是唯一会访问网络的命令；正常比较文档仍然完全离线。已存在的文件默认跳过，加 `--force` 才会重新下载。如果 `models/model.onnx` 已经是单文件大模型（便携包解压后就是这种结构），命令会跳过 `model.onnx_data`，不会额外下载约 95 MB 的外置权重。

#### 手动下载（内置命令不可用时）

**方式 B：从已有的便携包复制（离线，推荐）**

如果手边有本项目的便携包，模型已经在里面，不需要联网：

```powershell
# 解压便携包（按实际路径修改）
Expand-Archive .\dist\diffRact-0.2.0-win64.zip -DestinationPath .\dist\portable

# 建好 models 目录并复制
New-Item -ItemType Directory -Force .\models | Out-Null
Copy-Item .\dist\portable\diffRact-0.2.0-win64\models\* .\models\ -Force
```

复制后应得到：

```text
models/
  model.onnx
  tokenizer.json
```

这种方式拿到的是项目测试过的同一份模型，默认 `diffract.toml` 可以直接使用。

**方式 C：从 Hugging Face 下载 ONNX 转换**

`BAAI/bge-small-zh-v1.5` 的官方仓库主要是 PyTorch 权重；这里使用社区转换好的 ONNX 版本 `onnx-community/bge-small-zh-v1.5-ONNX`。它和 Transformers.js 兼容，输出 `last_hidden_state`，隐藏维度 512，适合本项目的 `pooling = "cls"`。

```powershell
python -m pip install -U huggingface_hub

# 国内网络如果访问 huggingface.co 失败，先设置镜像
$env:HF_ENDPOINT = "https://hf-mirror.com"

hf download onnx-community/bge-small-zh-v1.5-ONNX --local-dir models/bge-small-zh-v1.5-ONNX
```

下载完成后可以二选一：

1. 直接在 `diffract.toml` 指向下载目录：

   ```toml
   [model]
   path = "models/bge-small-zh-v1.5-ONNX/onnx/model.onnx"
   tokenizer_path = "models/bge-small-zh-v1.5-ONNX/tokenizer.json"
   ```

2. 或者复制成默认的扁平结构：

   ```powershell
   Copy-Item models\bge-small-zh-v1.5-ONNX\onnx\model.onnx .\models\model.onnx -Force
   Copy-Item models\bge-small-zh-v1.5-ONNX\onnx\model.onnx_data .\models\model.onnx_data -Force
   Copy-Item models\bge-small-zh-v1.5-ONNX\tokenizer.json .\models\tokenizer.json -Force
   ```

> `model.onnx` 和 `model.onnx_data` 必须放在同一目录。当前 ONNX Community 版本把权重放在外部数据文件里，只复制 `model.onnx`（约 41 KB）会导致模型加载失败。
>
> 如果下载到的是单文件 `model.onnx`（没有 `model.onnx_data`），只复制 `model.onnx` 和 `tokenizer.json` 即可。

**方式 D：浏览器手动下载**

打开 [onnx-community/bge-small-zh-v1.5-ONNX 文件页](https://huggingface.co/onnx-community/bge-small-zh-v1.5-ONNX/tree/main/onnx)（国内可用 [hf-mirror 镜像](https://hf-mirror.com/onnx-community/bge-small-zh-v1.5-ONNX/tree/main/onnx)），下载：

- `onnx/model.onnx`
- `onnx/model.onnx_data`
- 根目录的 `tokenizer.json`

按方式 C 的第 1 或第 2 种结构放置。

下载后建议用 `--require-semantic` 验证一次，模型不可用时会直接报错而不是静默降级：

```bat
diffract.exe --require-semantic --check 旧版.docx 新版.docx
```

输出中看到“语义匹配已启用（model.onnx，512 维）”即表示模型加载成功。若只需要纯文本比较，可以不下载模型，使用 `--no-semantic`。

配置步骤：

1. 在程序目录新建 `models` 目录，放入 `model.onnx` 和 `tokenizer.json`（也可放在任意位置）；
2. 在 `diffract.toml` 中确认路径：

   ```toml
   [model]
   path = "models/model.onnx"
   tokenizer_path = "models/tokenizer.json"
   enabled = true
   required = false
   max_seq_len = 512
   output_name = "last_hidden_state"
   pooling = "cls"
   dim = 0
   batch_size = 8
   ```

   相对路径的基准：默认读取工作目录下的 `diffract.toml`，路径相对工作目录；用 `-c` 指定配置文件时，路径相对该配置文件所在目录。

3. 也可以不改配置文件，直接用命令行覆盖：

   ```bat
   diffract.exe --model D:\models\model.onnx --tokenizer D:\models\tokenizer.json --pooling cls --dim 0 旧版.docx 新版.docx
   ```

4. `dim = 0` / `--dim 0` 表示按模型输出自动推断维度；`pooling` 可选 `mean`、`cls`、`model`（模型已自行池化）。BGE 系列推荐 `cls`。
5. 模型文件缺失或推理失败时，默认自动降级为纯文本匹配并在界面/报告注明原因；需要强制失败时用 `--require-semantic` 或设置 `required = true`（退出码 2）。

模型输入输出要求见下文“语义模型契约”。

## 命令行

```text
diffract.exe [OPTIONS] [OLD] [NEW]
diffract.exe download-model [OPTIONS]
```

`download-model` 只下载语义模型，不需要提供 docx：

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--dir <DIR>` | `models` | 模型输出目录 |
| `--mirror` | 关 | 使用 `https://hf-mirror.com` 镜像 |
| `--force` | 关 | 覆盖已存在的模型文件 |

比较命令的参数：

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `<OLD>` `<NEW>` | 必填 | 旧、新版本 .docx；TUI 模式下可省略，省略时进入文件选择器 |
| `--check` | 关 | 非交互模式，输出统计并按 0/1/2 退出 |
| `--html <FILE>` | 关 | 导出 HTML 报告；指定后进入非交互模式 |
| `--md <FILE>` | 关 | 导出 Markdown 报告（别名 `--markdown`）；指定后进入非交互模式 |
| `-c, --config <FILE>` | `./diffract.toml` | 指定配置文件；不存在时使用内置默认值 |
| `--window <N>` | 5 | 候选段落的搜索窗口（前后各 N 段） |
| `--weight-text <F>` | 0.6 | 文本相似度权重 |
| `--weight-semantic <F>` | 0.4 | 语义相似度权重 |
| `--threshold-modified <F>` | 0.75 | 确认修改阈值 |
| `--threshold-suspected <F>` | 0.45 | 疑似修改阈值 |
| `--text-candidate-threshold <F>` | 0.30 | 进入语义打分的文本相似度下限 |
| `--model <FILE>` | `models/model.onnx` | ONNX 模型路径 |
| `--tokenizer <FILE>` | `models/tokenizer.json` | tokenizer.json 路径 |
| `--max-seq-len <N>` | 512 | 单次编码最大 token 数，超长段落分块 |
| `--no-semantic` | 关 | 关闭语义匹配，只用文本相似度 |
| `--require-semantic` | 关 | 语义模型必须可用，加载失败按运行错误退出 2 |
| `--output-name <NAME>` | `last_hidden_state` | 模型输出张量名 |
| `--pooling <POOLING>` | `mean` | `mean` / `cls` / `model` |
| `--dim <N>` | 768 | 向量维度，`0` 表示按模型输出推断 |
| `--context-chars <N>` | 80 | 段内 diff 展示的上下文长度 |
| `--log <FILE>` | 不写日志 | 日志文件路径 |
| `--log-level <LEVEL>` | `info` | `off/error/warn/info/debug/trace` |

命令行参数优先于配置文件。`--no-semantic` 与 `--require-semantic` 互斥。

## 配置文件

查找顺序：

1. `-c/--config` 指定的文件；
2. 当前工作目录下的 `diffract.toml`；
3. 内置默认值。

配置文件中的相对路径按配置文件所在目录解析；没有配置文件时按当前工作目录解析。模板如下：

```toml
[similarity]
window = 5
weight_text = 0.6
weight_semantic = 0.4
threshold_modified = 0.75
threshold_suspected = 0.45
text_candidate_threshold = 0.30

[model]
path = "models/model.onnx"
tokenizer_path = "models/tokenizer.json"
max_seq_len = 512
enabled = true
required = false
output_name = "last_hidden_state"
pooling = "mean"          # mean / cls / model
dim = 768                 # 0 表示自动推断
batch_size = 8
input_ids_name = "input_ids"
attention_mask_name = "attention_mask"

[diff]
context_chars = 80

[log]
level = "info"
path = "diffract.log"     # 省略 path 则不写日志
```

约束（启动时校验，非法配置以退出码 2 结束）：

- `window > 0`；两个权重非负且不能同时为 0；
- 三个阈值都在 0～1，且 `threshold_suspected <= threshold_modified`；
- `context_chars > 0`；
- `max_seq_len >= 8`，`batch_size > 0`；
- `required = true` 时必须 `enabled = true` 且配置了模型路径。

## 判定规则

### 预处理

- 只读取 `word/document.xml` 中的正文；页眉、页脚、脚注、批注不参与比较。
- 正文段落和表格都会解析。表格先按表头对齐列，再按行比较，空行被丢弃；表格内的多段落单元格用换行合并。
- 合并单元格：纵向合并（`vMerge`）的内容复制到它覆盖的每一行；横向合并（`gridSpan`）只在起始列保留内容，其余列留空占位。
- 空段落和仅含空白的段落被丢弃，报告中的段号是丢弃后的 1 基序号。
- 段内手动换行、制表符保留在同一段中。
- 匹配时忽略行首编号（`第X章`、`第十二条`、`#` 等）、全半角差异、大小写、标点和多余空白；展示与导出始终使用原文。

### 配对与打分

1. 先全局查找归一化文本完全相同的段落；其中位置未变的记为“未变”，换了位置的记为“移动”。
2. 其余段落以完全匹配的段落为锚点预测位置，在预测位置前后 `window` 段内找候选。
3. 候选的文本相似度低于 `text_candidate_threshold` 时直接跳过；否则计算：
   - 文本相似度 = 0.5 × 字符三元组 Jaccard + 0.5 × jieba 分词重合度；
   - 语义相似度 = 两段句向量的余弦相似度；
   - 综合分 = `weight_text × 文本 + weight_semantic × 语义`。
4. 语义未启用时，综合分直接取文本相似度。
5. 双向贪心配对，取得分最高的唯一组合：
   - 综合分 ≥ `threshold_modified` → 修改；
   - `threshold_suspected` ≤ 综合分 < `threshold_modified` → 疑似修改；
   - 未配对 → 新增或删除。
6. 一段被拆成多段（或反之）按“删除 + 新增”记录，不强行判为修改。

修改和疑似修改会生成字符级 diff；TUI 中按 `Enter` 查看，默认上下文由 `context_chars` 控制。

### 表格比较

表格在段落配对之后单独处理，记录仍按正文顺序混排。

1. **表对齐**：按文档顺序做保序对齐，综合首列键重合度、整行文本重合度、行列数比例、标题相似度和位置；对不上的表按整表新增或删除处理（各一条记录，内容为整表逐行文本）。
2. **列对齐**：把首行视为表头；若首行只有一格（常见于跨列表格标题），则在其后两行中找第一行有多个非空单元格的作为表头。先按列名精确配对，再对“本月”→“本月完成”这类改名做模糊配对，最后按剩余顺序兜底。插列、删列、改列名、挪列都先对齐列，再比较单元格，避免整行错位后文本 diff 看花。无表头或表头重名无法唯一确定时退化为按位置对齐。
3. **序号列**：形如 `1、2、3…` 且从 1 开始连续递增的列按行位置处理，不做主键、也不参与文本比较；增删行后的整体重编号不会产生差异。记录正文同样不显示序号列，行位置仍由 `表N·R` 给出。
4. **行对齐**：取第一个在两表中都足够唯一的非序号列作为行主键，主键相同或整行文本完全相同的行直接配对，允许跨顺序，以便识别行移动；其余行按顺序做综合分（文本 + 语义）对齐，避免插入一行导致后面全部错位。
5. **判定**：
   - 主键相同的配对行，内容不同即判“修改”；
   - 内容完全相同但顺序变化，判“移动”；顺序变化且内容也改动，按“修改”并备注行序变化；
   - 其余配对行按综合分与 `threshold_modified` / `threshold_suspected` 判修改或疑似；多数单元格完全相同、只有个别值变化时，即使综合分偏低也按“修改”处理，并备注原因；
   - 未配对的行分别记为整行新增 / 删除。
6. **单元格差异**：修改和疑似修改的表格行只记录内容发生变化的单元格（按对齐后的列名标注旧值、新值），未变化的单元格不再重复展示；插列/删列时只有表头及该列实际有值的行进入记录，改列名时通常只有表头一行。合并单元格沿用解析时的展开结果：纵向合并的内容出现在其覆盖的每一行，横向合并的占位列不参与比较。
7. 未变的表格行只参与统计，不逐条列出；整表新增或删除只出一条记录，正文栏显示整表内容。
8. 表格行与段落共用同一套语义模型：用剔除序号列后的行文本计算句向量，综合分为 `weight_text × 文本 + weight_semantic × 语义`，未启用语义时退化为文本分。记录位置显示为 `表2·3`（第 2 张表第 3 行）。

## TUI 操作

不带 `--check` 时进入全屏界面；要求标准输入输出连接交互式终端。两个文件参数都省略时，先进入文件选择器；只给一个文件时只补选缺少的那个。

### 文件选择器

默认列出当前目录下的文件夹和 `.docx`；按 `Ctrl+T` 切换为显示全部文件。非 `.docx` 文件只在浏览时可见，确认时会被拒绝，避免进入比较阶段才报格式错误。第二步标题为“选择新版文件”，初始目录自动设为旧版所在目录，标题区显示已选旧版路径。

输入框同时承担筛选和路径输入：

- 普通文字按文件名做不区分大小写的子串筛选，空格按字面匹配；
- 以 `/`、`\`、盘符（如 `C:`）开头，或包含路径分隔符时按路径解析：分隔符前的部分用于定位目录，最后一段作为筛选词；`~` 表示用户主目录；
- 路径模式下的空格属于文件名，不再当作分隔符；
- 路径不存在或不可读时，在标题区显示原因并保留输入，修改后自动恢复列表。

| 按键 | 作用 |
| --- | --- |
| `↑` / `↓`、`PageUp` / `PageDown`、`Ctrl+P` / `Ctrl+N` | 选择条目 |
| `Enter` | 进入目录 / 确认文件 |
| `Tab` | 把选中项补全为完整路径，目录会补上分隔符 |
| `Backspace` | 删除输入；输入为空时返回上级目录 |
| `←` / `→` | 移动输入光标 |
| `Home` / `End` | 移动输入光标；输入为空时跳到首项 / 末项 |
| `Ctrl+U` | 清空输入 |
| `Ctrl+T` | 切换“仅 .docx” / “全部文件” |
| `Esc` | 第二步返回第一步；第一步取消并退出 |
| `Ctrl+C` | 随时退出选择器 |

当前版本使用普通子串筛选，不包含模糊匹配或拼音搜索。

界面右侧详情为左右两栏，分别显示旧文和新文：修改/疑似记录的两栏带字符级差异标记，新增只在新文栏显示，删除只在旧文栏显示。表格行的修改/疑似记录只列出有变化的单元格，按“列名：旧值 / 新值”逐行展示，未变化的单元格不重复出现。两栏各自独立滚动。列表与详情的宽度比例为 40% / 60%。记录列表的“旧位置 / 新位置”列显示段落序号（`#12`）或表格位置（`表2·3`）。

界面有“记录”和“详情”两级焦点：记录列表边框为青色时，方向键或 `j` / `k` 用于选择记录；按 `Enter` 进入详情后，详情边框变青色，当前栏标题前出现 `▸`。详情焦点下 `Tab` 在旧文、新文之间切换；新增记录只有新文栏、删除记录只有旧文栏，此时不会切换。按 `q` 或 `Esc` 返回记录列表。

顶部一行是筛选栏，从左到右依次为“全部 / 修改 / 疑似 / 新增 / 删除 / 移动”及各自条数；当前筛选项以青色底色高亮，右侧灰色显示未变条数和比较耗时。数字 `1`～`6` 可直接选中，`Tab` 或 `f` 循环切换。

| 按键 | 记录焦点 | 详情焦点 |
| --- | --- | --- |
| `↑` / `↓` / `k` / `j` | 上下选择 | 滚动当前栏 1 行 |
| `PageUp` / `PageDown` | 翻 10 条 | 滚动当前栏 10 行 |
| `Home` | 首条 | 当前栏回到顶部 |
| `End` | 末条 | — |
| `Enter` | 进入详情 | 展开或收起完整 diff |
| `Space` | 展开或收起完整 diff | 展开或收起完整 diff |
| `Tab` / `Shift+Tab` | 循环切换筛选 | 切换旧文 / 新文 |
| `f` | 循环切换筛选 | — |
| `1`～`6` | 全部 / 修改 / 疑似 / 新增 / 删除 / 移动 | — |
| `[` / `]` | — | 滚动当前栏 1 行 |
| `{` / `}` | — | 滚动当前栏 10 行 |
| `e` | 导出报告 | 导出报告 |
| `?` | 帮助 | 帮助 |
| `q` | 退出 | 返回记录列表 |
| `Esc` | 收起 diff；未展开时退出 | 返回记录列表 |
| `Ctrl+C` | 退出 | 退出 |

导出流程：按 `e` 选 HTML 或 Markdown（也可用 `h` / `m` 快捷选择），输入路径后按 `Enter`。默认文件名为 `diffract-report-年月日-时分秒.html|md`；支持相对路径和绝对路径，父目录会自动创建。

界面顶部会显示语义匹配状态：已启用时显示模型名和维度；未启用时以黄色提示原因（如模型文件不存在）。

## 导出报告

HTML 和 Markdown 包含：

- 旧、新文件名及段落数、表格数，语义匹配状态，六类计数，耗时；
- “确定差异”区：新增、删除、修改、移动；
- “疑似修改待确认”区：疑似修改；
- 每条记录含编号、状态、旧/新段号或表行位置、原文、文本/语义/综合分、`matched_to`、备注；段落修改附字符级 diff，表格修改附“列 / 旧值 / 新值”单元格差异表，只列出变化的单元格。

未变段落只计入统计，不逐条导出。HTML 为单文件、样式内嵌，可直接发送或打印。

## CI 模式与退出码

`--check` 把简报写到标准输出，不进入 TUI：

```
旧文件：old.docx（12 段，2 表）
新文件：new.docx（13 段，2 表）
语义匹配：语义匹配已启用（model.onnx，512 维）
结果：未变 10 | 修改 1 | 疑似 0 | 新增 1 | 删除 0 | 移动 1
结论：存在差异
耗时：0.42s
```

| 退出码 | 含义 |
| --- | --- |
| 0 | 两份文档无差异（全部未变） |
| 1 | 存在差异（修改/疑似/新增/删除/移动任一） |
| 2 | 运行错误（文件不存在、格式错误、配置非法、模型加载失败且为必需等） |

TUI 正常退出始终为 0；运行期错误为 2。

## 语义模型契约

- tokenizer：Hugging Face `tokenizer.json`，由 `tokenizers` 库直接加载。
- 输入：`input_ids`、`attention_mask`，均为 `int64`，形状 `[batch, seq]`，batch/seq 为动态轴；名称可在配置中修改。
- 若模型还声明 `token_type_ids` 输入（BERT 系导出常见），程序会自动补全同形状的全零 `int64` 张量，无需额外配置。
- 输出（名为 `output_name`，默认 `last_hidden_state`）：
  - 3D `[batch, seq, dim]`：配合 `pooling = "mean"`（按 attention mask 求平均）或 `"cls"`（取首 token）；
  - 2D `[batch, dim]`：配合 `pooling = "model"`，表示模型已自行池化。
- 仅使用 CPU 推理；`dim = 0` 时从模型输出形状推断维度。
- 超过 `max_seq_len` 的段落会切块推理，块向量取平均后再做 L2 归一化。
- 段落和表格行都会计算语义向量；表格行用剔除序号列后的行文本（单元格以 ` | ` 连接），空行不做语义打分。
- 加载或推理失败时：
  - 默认自动降级为纯文本匹配，界面顶部与导出报告均注明原因；
  - 配置 `required = true` 或使用 `--require-semantic` 时改为报错，退出码 2。

默认阈值适用于一般中文句向量模型，但不同模型的分数分布差异较大。建议先用一批已知结论的文档校准 `threshold_modified`、`threshold_suspected` 和 `text_candidate_threshold`。

## 日志

日志只写文件，不输出到 stderr/stdout，因此不会污染 TUI 或 `--check` 的管道输出。默认不写日志；设置 `[log].path` 或 `--log` 后启用，记录模型加载、相似度计算、耗时和错误。

## 已知边界

- 仅支持 `.docx`，不支持 `.doc`；加密或损坏的文件会报错。
- 表格记录仍以“行”为单位，一行内多处修改合并为一条记录，但记录内部只列出发生变化的单元格。
- 列对齐依赖表头；表头缺失、重名或整体重写时按剩余顺序兜底，极端情况下可能把“删一列 + 加一列”识成列改名。
- 嵌套表格的文字先不参与比较；表格内图片、公式、复选框等内容不比较。
- 横向合并的单元格只在首列保留内容，其余列为空占位；颜色、边框、列宽等表格格式变化不构成差异。
- 比较单位是段落和表格行；文字格式（字体、样式、颜色）变化不构成差异。
- 空段落被忽略，因此纯空行变化不会报告。
- 大范围重排且缺少完全相同的锚点时，移动识别可能退化为删除加新增。
- 文件选择器只列当前目录，不递归扫描；名称筛选是普通子串，不支持模糊匹配或拼音检索。

## 设计文档

- [项目总体设计](docs/design.md)

## 从源码构建

需要 Rust 1.92 或更新版本（edition 2024）以及可用的 C++ 构建环境（MSVC）：

```bat
cargo test
cargo build --release
```

产物为 `target/release/diffract.exe`。构建时会下载 Rust/ONNX Runtime 依赖；首次构建需要网络，之后可离线。正常比较不联网；只有显式执行 `download-model` 时会联网下载模型。

仓库包含 GitHub Actions 工作流（`.github/workflows/ci.yml`）：push / PR 时执行 `cargo fmt --check`、`cargo clippy`、`cargo test` 和 release 构建；推送 `v*` 标签时把 `diffract.exe`、README 与配置文件打成 zip 附加到 GitHub Release。CI 与发布包都不下载、不包含语义模型，使用时按上文“启用语义匹配”自行放置模型文件。
