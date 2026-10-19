# MacEvery Top300一致性修复与紧凑副本验收

2026-10-19，r1初稿。当前通过的是Rust检查和隔离后端验收；最终性能门禁、Swift真实传输、原生安装与新24小时观察由根节点继续闭合，本文不提前标记发布或长期稳定性通过。

## 问题与改动

原CLI回退及Low Memory在limit=300时只对最多24,000条SQLite候选排序，Fastest对完整内存索引排序，相同输入可以返回不同Top300。本轮保留评分、完整结果字段及稳定顺序，改为流式评分和有界TopK。SQL预筛须覆盖所有可能匹配项，包含UTF-8边界、完整扩展名候选、basename/path必要字面量并集及多词分数界；Fastest查询热路径保持原实现。

完整搜索暴露了原索引物理布局的冷读成本。增加显式 `optimize --output NEW_DB --json`：只读源，经VACUUM INTO创建新文件，quick_check后用有界双cursor按ID顺序比较完整FileRecord及数量，独立核对内容标记和控制配置。拒绝既有输出、sidecar及活动事务。该命令不在启动时自动执行，也不自行替换真实库。

基线是已冻结的0.1.3-g8096e0a App，CLI SHA256为 `422e776c583ff9769c613c5f915f1b5c4e993da4085fa434042b81bd82a710ee`。本次最终受测CLI SHA256为 `836a9e678254776be807d0cd2c7c46e00021db25d9ab6900572e8ff46b5cbe99`；后者是制品哈希，尚不代表已提交源码或已安装App。

## 已完成的正确性验收

Debug、Release各149项单测、各3项bootstrap CLI和3项optimize CLI通过，420事务差分与prefix更新集成通过；all-target严格Clippy、格式和release build通过。原145项检查及失败编译记录继续保留，最终结果单独记录在[staging-checks.json](evidence/staging-checks.json)。

| 输入 | 完整FileRecord逐行验证 | 六view完成 | 候选三路径与旧Fastest oracle | 候选15秒预算 |
|---|---:|---:|---|---|
| 30k截断对抗库 | 30,000条 | 6×24项 | 72项六字段及顺序全部一致 | PASS |
| 固定真实快照 | 7,226,486条 | 6×24项 | 72项六字段及顺序全部一致 | PASS |

证据：[30k摘要](evidence/equivalence-compact-30k.json)、[真实快照摘要](evidence/equivalence-compact-real.json)。两份证据均使用同一个候选生成的紧凑母库，再为两版本的CLI、HTTP memory、HTTP sqlite分别复制，严格串行执行。旧CLI/sqlite在30k及真实快照中仍分别留下8处、14处历史阶段差异；这些差异不是候选的允许例外。

每个view包含15类初始查询和重命名、修改、删除三批更新的9项复查。矩阵覆盖code300、过滤、glob、真正非连续fuzzy、Unicode、无命中、扩展名OR及排除、path、part与多词。六个固定探针不含code，不改变真实code匹配集合；探针终态直接与已提交临时DB核对。结果比较包含path、basename、kind、size、mtime、score六字段及原始数组顺序。

两版本memory的full reload始终为0，delta apply/attempt/变更ID累计为0、1、2、3；sqlite full reload/attempt为0、1、2、3。严格ready、非stale、记录数、错误状态和清理门禁均通过。实际timeout使该view失败，后续阶段保留NOT_RUN并关闭owned服务；总PASS同时要求完整结果门禁和候选全部查询不超15秒。源快照、二进制未变，临时view副本及owned进程组/端口已清理，准备源与紧凑输出按约定保留。

真实快照在整理前后均由同一个旧Fastest全量oracle执行。全部24项的结果数量及六字段有序摘要完全相同，见[原布局与紧凑布局对照](evidence/original-vs-compact-memory.json)。这份矩阵证明查询及更新输出保持；全部FileRecord的等价性另外由维护CLI的全量双cursor验证承担，不能只用24项摘要代替。

## 原布局拒绝记录与维护成本

未整理原布局的正式全矩阵仍为 **REJECTED_COLD_IO**：候选memory、sqlite各24项完成且结果正确，但CLI的rare glob在独立新副本15.014秒超时；sqlite glob14.532秒、无命中14.857秒也接近预算。失败及NOT_RUN记录保留在[原布局拒绝证据](evidence/rejected-real-cold-1.json)，未延长查询预算或用热缓存覆盖失败。更早的[完整流式原型](evidence/rejected-full-stream-exploration.json)、[第二原型](evidence/rejected-second-exploration.json)与[第三原型](evidence/rejected-third-exploration.json)拒绝记录继续保留。

同列新dense索引的独立扫描为12.719→0.591秒；只REINDEX后仍11.650秒，未采纳。VACUUM新副本同名索引扫描0.590秒，支持物理布局是该次冷读代价的重要因素；该定位不等于所有查询耗时均由碎片单独决定。

最终显式维护命令在真实快照上的成本如下，均在查询计时之外：

| 项目 | 观测值 |
|---|---:|
| CLI wall（含整理、quick_check及全量记录核对） | 28.087秒 |
| CLI CPU | 13.852秒 |
| CLI进程生命周期HWM | 105,922,560字节，约101.02MiB |
| 含driver独立哈希/配置核验的准备wall | 34.229秒 |
| 主文件大小 | 5,410,697,216→4,896,702,464字节 |
| 完整核对记录 | 7,226,486条 |

源副本主文件SHA前后一致，内容ID/revision及roots/excludes/meta保持；仅公开数量和摘要，不公开配置路径或内容ID。维护源副本、输出及失败原始材料保留在本地私有raw证据中。

物理整理只执行一次，随后两版本共用相同新布局。查询收益包含代码和物理布局条件变化，不能当作纯代码提升。整理及校验也会触页；复制新文件不等于清空系统缓存。本矩阵是一次顺序A/B机制验收，CLI CPU包含进程启动/初始化/编码，HTTP CPU包含刷新/编码，RSS采样可能漏峰值，生命周期HWM来自wait4。独立同输入性能A/B/A结论须另行接入。

## 待闭合项目

| 项目 | 本稿状态 |
|---|---|
| Fastest code及短词CPU/RSS独立性能门禁 | PENDING，根节点接入正式证据 |
| 最终FSEvents更新验收 | PENDING，根节点接入正式证据 |
| 最终Swift真实15秒超时、取消及三模式集成 | PENDING |
| 源码提交/推送及精确SHA回读 | PENDING |
| 候选App构建、签名、安装与原生回读 | PENDING |
| 新24小时固定版本观察 | NOT_STARTED |

原48小时窗口已因本轮新任务停止，67个旧样本及state封存，不能拼入新窗口或称长期通过。最终安装须保留原App与真实索引回滚材料，验证等价副本和已测App后再替换；本地后端PASS不代替原生验收、正式QA或发布完成。
