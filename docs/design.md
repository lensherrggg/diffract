# diffRact 设计文档

- 状态：已实现
- 适用版本：diffRact 0.1.0
- 更新日期：2026-10-24
- 文档范围：整个项目的架构、数据模型、比较算法、输出方式和工程约束

## 1. 项目定位

diffRact 是一个完全本地运行的 `.docx` 版本比较工具。它把两份 Word 文档中的正文段落和表格行分别配对，输出六类结果：

| 状态 | 含义 |
| --- | --- |
| 未变（unchanged） | 归一化后文本完全相同，且位置未变 |
| 修改（modified） | 找到对应项，综合相似度达到确认阈值 |
| 疑似修改（suspected_modified） | 找到对应项，相似度介于疑似与确认阈值之间，需要人工确认 |
| 新增（added） | 新文档中没有对应项 |
| 删除（removed） | 旧文档中的对应项在新文档中不存在 |
| 移动（moved） | 归一化后文本完全相同，但位置发生变化 |

典型使用场景：

- 制度、方案、合同等文档改版时快速定位变化；
- 在 CI 或批处理中判断两份文档是否存在差异；
- 生成可归档、可发送的 HTML/Markdown 对比报告；
- 需要离线处理、不希望文档上传到外部服务的场景。

### 非目标

- 不支持 `.doc`、加密文档或损坏文件；
- 不做像素级/版式级比较，字体、颜色、边框、列宽等格式变化不构成差异；
- 不判断改动意图，只按文本和结构相似度给出“修改/疑似修改”；
- 不提供服务器、Web 界面或多人协作能力；
- 正常比较不访问网络，模型文件需要用户自行准备；只有显式的 `download-model` 子命令会联网。

## 2. 设计原则

1. **本地优先**：文档解析、相似度计算、报告生成全部在本机完成；正常比较无网络访问，只有显式 `download-model` 会下载模型。
2. **匹配与展示分离**：归一化文本只用于匹配，报告和界面永远展示原文。
3. **启发式可解释**：相似度由字符、分词和语义三部分构成；阈值、权重、窗口都可通过配置文件或命令行调整。
4. **宁可疑似，不可漏报**：低于确认阈值但仍有较高相似度的配对保留为“疑似修改”，交给人工判断。
5. **优雅降级**：语义模型缺失或推理失败时默认退化为纯文本匹配，并在 TUI/报告中明确提示；需要强制失败时使用 `--require-semantic`。
6. **输出可管道化**：`--check` 只输出稳定摘要并用 0/1/2 退出码表达结果，日志只写文件，不污染 stdout/stderr。
7. **报告安全**：所有进入 HTML 的文本都经过转义，避免文档内容被当成脚本执行。

## 3. 总体架构

```text
                         ┌──────────────┐
                         │   main.rs    │  CLI 解析、运行模式选择、退出码
                         └──────┬───────┘
                                │
              ┌─────────────────┼─────────────────┐
              ▼                 ▼                 ▼
        ┌──────────┐     ┌────────────┐    ┌──────────┐
        │ config.rs│     │ logging.rs │    │  tui/    │
        │ 配置校验 │     │ 文件日志   │    │ 交互界面 │
        └────┬─────┘     └────────────┘    └────┬─────┘
             │                                    │
             └──────────────┬─────────────────────┘
                            ▼
                     ┌────────────┐
                     │ engine.rs  │ 比较流水线编排、进度、汇总
                     └─────┬──────┘
          ┌────────────────┼────────────────┐
          ▼                ▼                ▼
   ┌──────────┐    ┌──────────────┐   ┌──────────┐
   │ docx/    │    │ matching/    │   │ model/   │
   │ 文档解析 │    │ 段落/表格匹配│   │ ONNX 向量│
   └────┬─────┘    └──────┬───────┘   └────┬─────┘
        │                 │                 │
        ▼                 ▼                 ▼
   ┌──────────┐    ┌──────────────┐   ┌──────────┐
   │normalize │    │ text_sim.rs  │   │ diff.rs  │
   │ 归一化   │    │ 文本/语义相似│   │ 字符 diff│
   └──────────┘    └──────────────┘   └────┬─────┘
                                            │
                                            ▼
                                     ┌────────────┐
                                     │ result.rs  │ 记录/状态/汇总
                                     └─────┬──────┘
                                           │
                              ┌────────────┴────────────┐
                              ▼                         ▼
                        ┌──────────┐             ┌──────────┐
                        │ report/  │             │  tui/    │
                        │ HTML/MD  │             │ 展示/导出│
                        └──────────┘             └──────────┘
```

### 模块职责

| 模块 | 职责 |
| --- | --- |
| `src/main.rs` | 定义 CLI；加载配置和日志；选择非交互/TUI 模式；映射退出码 |
| `src/config.rs` | TOML 配置结构、默认值、CLI 覆盖、相对路径解析、启动校验 |
| `src/logging.rs` | 文件日志实现；不向 stderr/stdout 写日志 |
| `src/docx/mod.rs` | 读取 `word/document.xml`，解析正文段落、表格、合并单元格、表标题 |
| `src/normalize.rs` | 仅用于匹配的 NFKC、大小写、标点、空白和段首编号归一化 |
| `src/matching/mod.rs` | 段落级精确匹配、窗口候选、拆分/合并启发式、双向贪心配对、记录排序 |
| `src/matching/text_sim.rs` | 字符 3-gram、jieba 分词、Jaccard、余弦相似度 |
| `src/matching/table.rs` | 表对齐、列对齐、行对齐、序号列、单元格差异、整表记录 |
| `src/model/embedder.rs` | ONNX Runtime 会话、tokenizer、分块/批处理、池化、向量归一化 |
| `src/model/download.rs` | `download-model` 子命令：从 Hugging Face/镜像下载模型，流式写盘、进度、跳过/覆盖 |
| `src/diff.rs` | 基于 `similar` 的字符级 diff，以及 TUI 上下文裁剪 |
| `src/result.rs` | 状态、分数、差异 span、表格/单元格差异、记录、汇总、比较输出 |
| `src/engine.rs` | 解析 → 语义准备 → 段落匹配 → 表格匹配 → 排序编号 → 字符 diff → 汇总 |
| `src/report/mod.rs` | `--check` 文本摘要、单文件 HTML、Markdown 报告 |
| `src/tui/mod.rs` | ratatui 界面、后台比较线程、筛选、详情滚动、导出对话框 |

## 4. 数据模型

### 4.1 文档模型

```rust
struct Document {
    paragraphs: Vec<Paragraph>,
    tables: Vec<Table>,
}

struct Paragraph {
    index: usize,      // 过滤空段后的 0 起始序号
    text: String,      // 原文
    normalized: String // 仅用于匹配
}

struct Table {
    index: usize,          // 全文 0 起始表号
    caption: Option<String>,
    anchor: usize,         // 表格前的非空段落数，用于记录排序
    rows: Vec<TableRow>,
}

struct TableRow {
    index: usize,          // 表内 0 起始行号，空行已剔除
    cells: Vec<String>,    // 已展开的单元格
    text: String,          // " | " 连接后的展示文本
    key: Option<String>,   // 首列归一化文本，用于表级/行级粗匹配
}
```

### 4.2 结果模型

```rust
enum Status {
    Unchanged, Added, Removed, Modified, Moved, SuspectedModified,
}

struct Scores {
    text: f64,
    semantic: Option<f64>,
    total: f64,
}

struct ParaDiff {
    old: Vec<DiffSpan>, // Equal + Delete
    new: Vec<DiffSpan>, // Equal + Insert
}

struct CellChange {
    column: String,
    old: Option<String>,
    new: Option<String>,
    diff: Option<ParaDiff>,
}

struct Record {
    id: usize,
    status: Status,
    old_para_index: Option<usize>,
    new_para_index: Option<usize>,
    old_text: Option<String>,
    new_text: Option<String>,
    scores: Scores,
    diff: Option<ParaDiff>,
    row_diff: Option<Vec<CellChange>>,
    matched_to: Option<usize>,
    note: Option<String>,
    table: Option<TableLocation>,
    order: f64, // 仅内部排序，不序列化
}

struct CompareOutput {
    old_path: PathBuf,
    new_path: PathBuf,
    old_paragraphs: usize,
    new_paragraphs: usize,
    old_tables: usize,
    new_tables: usize,
    records: Vec<Record>,
    summary: Summary,
    semantic: SemanticStatus,
    elapsed: Duration,
}
```

`Record` 是段落和表格行共用的统一结果单元。段落记录用 `old_para_index/new_para_index` 定位；表格记录用 `TableLocation` 定位，并用 `row_diff` 承载单元格级差异。未变记录和整表新增/删除记录也走同一模型，便于统计和排序。

## 5. 比较流水线

`engine::compare_files` 是唯一的比较入口，顺序如下：

```text
解析旧/新文档
  → 可选加载语义模型并计算向量
  → 段落匹配
  → 表格匹配
  → 合并记录并按文档顺序排序
  → 分配记录编号
  → 为修改/疑似记录生成字符级 diff
  → 统计汇总
  → 返回 CompareOutput
```

### 5.1 文档解析

实现：`src/docx/mod.rs`。

1. 用 `zip` 打开 `.docx`，只读取 `word/document.xml`，因此页眉、页脚、脚注、批注不参与比较。
2. 用 `quick_xml` 流式解析，只处理正文层级的 `w:p`、`w:tbl`、`w:tr`、`w:tc`。
3. 段落文本保留 `w:tab`（制表符）、`w:br`/`w:cr`（手动换行）、`noBreakHyphen`；修订删除的 `w:delText` 被跳过，修订插入的 `w:t` 保留。
4. 表格行内每个单元格收集其多段文本，用换行合并；空行被丢弃。
5. 纵向合并（`vMerge`）：续格从上一行同列复制内容，使合并内容出现在其覆盖的每一行。
6. 横向合并（`gridSpan`）：只在起始列保留文本，其余列插入空占位，因此后续列的索引仍然稳定。
7. 嵌套表格的文字不进入外层单元格：`table_depth == 1` 才收集单元格文本，嵌套层的内容被忽略。
8. 表格上方的段落如果形如“表1 …”“附表2 …”“续表…”，作为表标题（caption）保存，仅用于展示和表对齐打分。

解析后，每个段落生成原文和 `normalize_for_match` 归一化文本；每张表生成行、首列 key、表标题和正文锚点。

### 5.2 归一化

实现：`normalize_for_match`。

- NFKC 统一全半角、兼容字符；
- 转小写；
- 剥离段首编号：`第十二条`、`第三条`、`一、`、`（三）`、`1.`、`1.2`、列表符号等；
- 常见中文标点映射为 ASCII 标点，并去掉标点前的空格；
- 折叠空白，去掉零宽字符；
- 结果只用于匹配，不进入任何展示或报告。

其中“剥编号”只处理段首常见形态，避免误伤正文中的年份、比例等数字。

### 5.3 语义向量

实现：`engine::prepare_semantic` + `model::Embedder`。

模型文件不在 git 中。首次使用先执行 `diffract.exe download-model`（或 `--mirror`），把 `model.onnx`、外置权重 `model.onnx_data` 和 `tokenizer.json` 放到 `models/`；该命令是运行期唯一的联网入口。如果已有单文件 `model.onnx`（便携包结构），会跳过 `model.onnx_data`，避免重复下载。

1. 只有 `[model].enabled = true` 时尝试加载。
2. 检查模型和 tokenizer 路径；加载 ONNX Runtime 会话，图优化 Level3，线程数取 CPU 并行度并限制在 1～8。
3. tokenizer 使用 Hugging Face `tokenizer.json`；自动识别 `[CLS]/[SEP]/[PAD]` 或 `<s>/</s>/<pad>`。
4. 收集旧/新段落归一化文本和表格行文本，按文本去重，避免重复推理。
5. 唯一文本按 64 条一组送入 embedder；embedder 再按 `[model].batch_size` 分 batch。
6. 超过 `max_seq_len` 的文本按 token 切块；每块单独推理，块向量取平均后再做 L2 归一化。
7. 池化支持：
   - `mean`：按 attention mask 对 3D `[batch, seq, dim]` 输出求平均；
   - `cls`：取首 token；
   - `model`：模型已输出 2D `[batch, dim]` 向量。
8. 如果模型声明 `token_type_ids` 输入，自动补全同形状全零 `int64` 张量。
9. 加载/推理失败时：
   - `required = false`：记录原因，退化为纯文本相似度；
   - `required = true` 或 `--require-semantic`：返回错误，退出码 2。

语义向量按“段落数组下标”和“表/行下标”保存，供匹配阶段按索引取用。

### 5.4 段落匹配

实现：`matching::match_documents`。

#### 第一步：精确匹配

1. 建立新文档 `normalized → new_index 列表` 的索引。
2. 按旧文档顺序，为每个旧段落找同文本的新段落，按出现顺序一一消费。
3. 所有精确匹配按旧下标排序，对新下标求最长递增子序列（LIS）。
4. LIS 成员且 `old_index == new_index` 的记为“未变”；其余精确匹配记为“移动”。
5. 精确匹配同时作为锚点，用于预测后续段落在新文档中的位置。

`insertion_does_not_mark_everything_moved` 测试保证：在开头插入一段不会让后面所有段落都被判为移动。

#### 第二步：窗口候选 + 相似度

对每个未匹配旧段落：

1. 用前后最近的精确锚点预测新位置，取 `predicted ± window` 作为候选范围。
2. 计算文本相似度；低于 `text_candidate_threshold` 的候选直接跳过。
3. 如果语义可用，计算余弦相似度，综合分为：
   `weight_text × text + weight_semantic × semantic`；否则综合分等于文本分。
4. 综合分低于 `threshold_suspected` 的候选丢弃。

#### 第三步：拆分/合并启发式

一个旧段落如果同时与两个相邻新段落高度相似，且长度关系接近“拆分”，则不参与后续配对，最终表现为“删除 + 新增”。反向同理处理“合并”。这样避免把一段拆成两段强行判成修改。

#### 第四步：双向唯一贪心

候选按以下顺序排序：

1. 综合分降序；
2. 文本分降序；
3. 新旧位置距离升序；
4. 旧下标、新下标升序。

依次取候选，若旧、新任一侧已被占用则跳过。这样保证一对一配对，并优先保留分数最高、位置最近的组合。

配对成功：

- 综合分 ≥ `threshold_modified` → 修改；
- 否则 → 疑似修改；
- 未配对的旧段落 → 删除；
- 未配对的新段落 → 新增。

#### 第五步：排序

- 有 `new_para_index` 的记录按新文档位置排序；
- 删除记录按其旧位置对应的新位置插入到相邻记录之间；
- 最终所有记录混合排序，`engine` 再统一分配 1 起始编号。

### 5.5 表格匹配

实现：`matching::table`。表格先做表对齐，再做列对齐，最后按行比较并生成单元格差异。

#### 表对齐

1. 对旧表序列和新表序列做保序 DP 对齐。
2. 两张表的配对分数由五部分组成：
   - 首列 key 集合 Jaccard；
   - 行签名集合 Jaccard（行内单元格排序后拼接，降低列顺序影响）；
   - 行列数比例；
   - 表标题相似度；
   - 文档位置接近程度。
3. 分数低于 `0.35` 的表不配对；未配对的表按“整表新增/删除”各生成一条记录。

#### 序号列

形如 `1、2、3…` 且从 1 开始连续递增的列被识别为序号列：

- 不作为行主键；
- 不参与文本/语义比较；
- 增删行后的整体重编号不会产生差异；
- 记录正文中不显示序号列。

#### 列对齐

- 识别表头行；若首行是跨列合并标题，则使用下一行；
- 先按列名精确匹配，再按字符集合 Jaccard 模糊匹配改名，最后按剩余顺序兜底；
- 新增列/删除列在对齐后的列序列中表现为一侧空单元格。

#### 行对齐

1. 选第一个在两表中都足够唯一的非序号列作为主键；主键相同或整行文本完全相同的行直接配对，允许跨顺序。
2. 其余行做保序 DP，避免插入一行导致后面全部错位。
3. 用 LIS 判断配对是否乱序，区分“插入导致顺移”和“真正的行移动”。

#### 状态判定

- 对齐后行文本相同：顺序变化记“移动”，否则不记录；
- 有单元格变化：
  - 主键相同，或综合分 ≥ `threshold_modified`，或多数单元格相同 → 修改；
  - 否则 → 疑似修改；
- 未配对行 → 整行新增/删除；
- 整表新增/删除 → 一条记录，正文为整表逐行文本。

#### 单元格差异

行配对成功后，`row_cell_changes` 按列对齐结果逐列比较单元格：

1. 取旧表对应列和新表对应列的值并 `trim`；
2. 两侧相同，或两侧都为空，跳过；
3. 否则写入 `CellChange { column, old, new, diff }`，空字符串转为 `None`。

列名优先取新表表头，其次旧表表头，最后回退为“第N列”。因此：

- 新增列/删除列表现为一侧为 `None`；
- 改列名只影响表头单元格；
- 插列、删列不会导致后续列整体错位；
- 一行内多处修改仍合并为一条表格记录，但记录内部只列变化单元格。

`engine` 在记录排序后为每个双侧都有值的单元格生成字符级 diff；报告和 TUI 优先渲染 `row_diff`，不再输出整行文本 diff。纵向合并的内容按解析阶段展开到覆盖的每一行，横向合并的占位列内容为空。

### 5.6 字符级差异

实现：`src/diff.rs`。

- 使用 `similar::TextDiff::from_chars` 做字符级 diff；
- 生成旧侧 `Equal + Delete` 和新侧 `Equal + Insert` 两套 span；
- 相同操作的相邻 span 合并，减少碎片；
- `excerpt` 只保留第一处到最后一起变更前后 `context_chars` 个字符，用于 TUI 非展开模式；
- `engine` 只为“修改/疑似”记录生成整行 diff；如果记录带 `row_diff`，再为每个双侧都有值的单元格生成独立 diff。

### 5.7 汇总与编号

- `Summary::from_records` 统计六类数量；
- `Summary::has_differences()` 作为 `--check` 退出码 0/1 的依据；
- 记录按 `order` 排序后统一编号；
- `CompareOutput` 携带文件名、段落/表格数、记录、汇总、语义状态和耗时。

## 6. 相似度与判定

### 文本相似度

`text_similarity` 由两部分各占 50%：

1. **字符 3-gram Jaccard**：对归一化文本取连续 3 个字符的哈希集合，计算交集/并集；
2. **jieba 分词词 Jaccard**：只保留包含字母或数字的 token，对 token 哈希集合计算 Jaccard。

短于 3 个字符的文本退化为整段哈希。Jieba 通过 `OnceLock` 全局懒加载，避免重复初始化。

### 语义相似度

向量先做 L2 归一化，`cosine` 即点积；长度不一致或空向量返回 0。

### 综合分

- 语义可用：`weight_text × text + weight_semantic × semantic`，裁剪到 `[0,1]`；
- 语义不可用：综合分 = 文本分。

### 阈值

| 参数 | 默认值 | 作用 |
| --- | ---: | --- |
| `window` | 5 | 候选段落搜索窗口 |
| `weight_text` | 0.6 | 文本权重 |
| `weight_semantic` | 0.4 | 语义权重 |
| `threshold_modified` | 0.75 | 确认修改阈值 |
| `threshold_suspected` | 0.45 | 疑似修改阈值 |
| `text_candidate_threshold` | 0.30 | 进入语义打分的文本下限 |

表格行复用同一套阈值；默认配置面向一般中文句向量模型，换成其他模型时需要重新校准。

## 7. CLI、配置与运行模式

### 命令行

```text
diffract.exe [OPTIONS] <OLD> <NEW>
diffract.exe download-model [--dir <DIR>] [--mirror] [--force]
```

`download-model` 是显式的模型准备命令：默认从 Hugging Face 下载 `onnx-community/bge-small-zh-v1.5-ONNX` 到 `models/`，`--mirror` 使用 hf-mirror.com，已存在文件默认跳过。该子命令是唯一会访问网络的运行路径。

主要参数：

- `--check`：非交互统计；
- `--html <FILE>` / `--md <FILE>`：导出报告，隐含非交互；
- `-c/--config <FILE>`：配置文件；
- 相似度/阈值/窗口覆盖；
- 模型路径、tokenizer、`max_seq_len`、输出名、池化方式、维度；
- `--no-semantic` / `--require-semantic`；
- `--context-chars`、`--log`、`--log-level`。

完整参数表见 README。

### 配置加载优先级

1. `-c/--config` 指定的文件；文件不存在直接报错；
2. 当前工作目录的 `diffract.toml`；
3. 内置默认值。

配置文件中的相对路径（模型、tokenizer、日志）相对于配置文件所在目录解析；没有显式配置文件时相对于工作目录。CLI 参数最后覆盖配置文件。

### 启动校验

`Config::validate` 在比较前检查：

- `window > 0`；
- 文本/语义权重非负且不能同时为 0；
- 三个阈值都在 0～1，且 `threshold_suspected <= threshold_modified`；
- `context_chars > 0`；
- 模型启用时 `max_seq_len >= 8`、`batch_size > 0`、输出名/输入名非空；
- `required = true` 时必须 `enabled = true` 且配置模型路径；
- 日志级别合法。

非法配置以退出码 2 结束。

### 运行模式

- 指定 `--check`、`--html` 或 `--md`：非交互模式；比较完成后输出摘要，并按需导出报告；
- 否则进入 TUI；如果标准输出不是终端，直接报错并提示使用 `--check` 或导出参数。

### 退出码

| 退出码 | 含义 |
| --- | --- |
| 0 | 无差异（全部未变） |
| 1 | 存在差异（修改/疑似/新增/删除/移动任一） |
| 2 | 运行错误（文件、格式、配置、模型必需但加载失败等） |

TUI 正常退出始终为 0；运行期致命错误为 2。

## 8. 输出与交互

### 8.1 `--check`

输出旧/新文件名、段落数、表格数、语义匹配状态、六类计数、结论和耗时。只在 stderr 是终端时显示进度，避免污染管道输出。

### 8.2 HTML / Markdown

实现：`report::export_html` / `export_markdown`。

报告包含：

- 文件信息、生成时间、耗时、语义状态；
- 六类计数；
- “确定差异”区：新增、删除、修改、移动；
- “疑似修改待确认”区：疑似修改；
- 每条记录的编号、状态、位置、文本/语义/综合分、`matched_to`、备注；
- 段落修改的字符级 diff；
- 表格修改的“列 / 旧值 / 新值”单元格差异表。

HTML 是单文件、内嵌样式，适合直接浏览、发送或打印。所有文本经过 HTML 转义；Markdown 输出对表格单元格中的 `|` 做转义。

### 8.3 TUI

实现：`src/tui/mod.rs`。

- 启动时进入备用屏幕和 raw mode，`TerminalGuard` 在 Drop/panic 时恢复终端；
- 比较在独立线程执行，通过 `mpsc` 把 `Progress`、`Done`、`Failed` 发回 UI；
- UI 每 100ms 轮询事件，比较期间显示阶段和向量进度；
- 记录列表显示状态、旧/新位置、内容预览；
- 详情分旧文/新文两栏，修改/疑似记录显示字符级 diff 或单元格差异；
- 筛选：全部/修改/疑似/新增/删除/移动，支持数字键、Tab 循环；
- 记录焦点与详情焦点分离，详情支持左右栏切换、滚动、展开/收起；
- `e` 打开导出对话框，选择 HTML/Markdown 并输入路径；
- 顶部显示语义匹配状态；未启用时黄色提示原因；
- 日志只写文件，不写 stderr，避免破坏界面。

## 9. 错误处理、日志与安全

- 所有文件/解析/配置错误通过 `anyhow::Context` 增加路径和操作上下文，最终由 `main` 打印 `错误：{err:#}` 并返回 2。
- 语义模型不是必需时，加载或推理错误转为 `SemanticStatus::disabled(reason)`，在 TUI 和报告中展示原因。
- 日志通过 `log` 门面写入文件；`--log`/`[log].path` 未配置时完全关闭。
- HTML 输出的所有动态文本（文件名、正文、备注、列名、diff span）都经过 `escape_html`。
- 不执行文档中的宏、脚本或外部链接；正常比较无网络请求，只有显式 `download-model` 会访问模型仓库。

## 10. 性能设计

- **解析**：流式 XML，内存中只保留正文段落和表格文本；嵌套表格内容跳过。
- **段落匹配**：先精确匹配，再只在锚点预测窗口内计算相似度，避免 O(n²) 全量比较。
- **语义向量**：对旧/新全部文本去重后统一推理；按 64 条分组和 `batch_size` 批处理；同一文本只计算一次。
- **表格**：表数量通常很少，表对齐和行对齐的 DP 规模可控；列对齐只做 O(列数²) 的表头比较。
- **字符 diff**：只为修改/疑似记录生成，未变记录不计算。
- **TUI**：比较线程与渲染线程分离，界面在长时间语义推理时仍可显示进度。
- **模型推理**：图优化 Level3，线程数限制在 1～8，避免占满机器。

## 11. 测试与 CI

### 单元测试

当前测试覆盖：

- 归一化：编号剥离、全半角、标点、空白、CJK 空格；
- docx 解析：段落/表格顺序、合并单元格、横向占位、嵌套表格、修订删除、表标题；
- 文本相似度：相同文本、近似改写、余弦；
- 段落匹配：未变/新增/删除/移动、插入不误判移动、阈值判定、窗口偏移、拆分/合并、编号忽略、删除记录插序；
- 表格匹配：表对齐、行增删改移、序号列、主键、语义分、列对齐、单元格差异；
- 报告：HTML 转义、脚本注入、单元格差异渲染；
- TUI：筛选循环、数字快捷键、详情换行缓存时机；
- CLI：导出参数、Markdown 别名、`download-model` 子命令解析；
- 模型下载：`.part` 路径、进度单位格式化。

运行：

```bat
cargo test
```

### CI

`.github/workflows/ci.yml` 在 push/PR 时执行：

1. `cargo fmt --check`；
2. `cargo clippy --all-targets`；
3. `cargo test`；
4. `cargo build --release`；
5. 打包 `diffract.exe`、README 和配置文件为 zip（不包含模型）。

推送 `v*` 标签时，将 zip 附加到 GitHub Release。

## 12. 已知限制

- 仅支持 `.docx`，不支持 `.doc`；加密或损坏文件报错。
- 只比较正文，页眉、页脚、脚注、批注、图片、公式、复选框不参与比较。
- 表格记录仍以行为单位，一行内多处修改合并为一条记录；记录内部只列变化单元格。
- 嵌套表格文字不参与比较。
- 横向合并只在首列保留内容，其余列为空占位；颜色、边框、列宽等格式变化不构成差异。
- 列对齐依赖表头；表头缺失、重名或整体重写时按位置兜底，极端情况下可能把“删一列 + 加一列”识别为列改名。
- 合并单元格内容变化按覆盖的每一行分别报告，暂不聚合为一条。
- 空段落被忽略，因此纯空行变化不会报告。
- 大范围重排且缺少完全相同锚点时，移动识别可能退化为删除加新增。
- 语义模型并非通用事实判断器，阈值需要用真实文档校准。

## 13. 演进方向

- 合并单元格变化按合并区域聚合；
- 支持多级表头，按列路径生成列名；
- 增加 JSON 输出，便于其他工具消费；
- 对语义向量做磁盘缓存，减少重复比较耗时；
- 改进拆分/合并识别，减少“删除 + 新增”；
- 增加跨平台发布包与安装说明；
- 对超大文档增加增量/流式比较策略。

## 14. 相关文档

- [README.md](../README.md)：安装、使用、配置与 CLI 参考。
