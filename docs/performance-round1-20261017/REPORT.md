# MacEvery 第一轮性能优化与正确性验证

日期：2026-10-17；版本：r1（本轮本地验证完成，待共享，未替换运行应用）。

## 范围与结论边界

本轮优化 watcher 提前排除与索引范围清理、查询预处理、Top-K、过期查询治理和搜索并发。保持原 memory 默认后端、原排序规则、limit 语义及各后端既有候选策略。未调整常驻索引结构，未替换当前运行 App，未写用户真实索引，未提交或推送，未正式打包发布。

## 实现

- watcher 在 FSEvents callback 过滤普通排除目录事件，恢复标志仍进入处理。scanner 预编译排除规则；目录清理使用 BINARY 路径范围索引，避免 LIKE 全表扫描及大小写误删。
- 只有明确不存在（NotFound / NotADirectory）才删除；权限、I/O 等失败保留记录并退避重试。未完成扫描只重试失败子树，恢复后才完成延后清理与 checkpoint。旧失败不阻塞新正常事件。批次最长 1 秒或 4,096 个事件，路径合并不再全量两两比较。
- 每次查询预解析 terms、lowercase、glob/fuzzy；有界堆只保留 K 个候选，最后才物化结果；相同排序键保留原输入顺序。
- GUI query 改变立即使旧 generation 失效，取消 HTTP / CLI；CLI 串行执行，并行读取 stdout/stderr，支持超时与回收。429/503 有限退避，不用 CLI 绕过服务背压。
- 服务单独限制搜索计算并发，默认 2，可通过 MACEVERY_MAX_ACTIVE_SEARCHES 调整；健康和状态请求不占搜索槽。API limit 不变。

## 正确性验证

- Rust 55 项通过，strict clippy all-targets 与变更文件格式检查通过。
- 独立旧评分与全量稳定排序 oracle：1,230 组完整结果差分，10,865 次单条评分对照；覆盖 Unicode、glob、fuzzy、过滤器、同键顺序、0/边界/极大 limit。
- 目录生命周期、排除不入队、路径范围 SQL 计划、失败重试、长期失败时新事件更新、部分扫描恢复与延后清理均有定向回归。
- 扫描 generation 在同进程内单调递增，回拨测试保证恢复记录不会被旧代际延后清理误删；没有引入全库 MAX 查询。跨进程历史时钟偏差仍为既有边界。
- 生产 Swift 源码编译及受控 GUI 回归通过：大 stdout/stderr、输出上限、超时、取消、清空、过期 fallback、背压与串行子进程回收。此为 model/URLProtocol/真实假 CLI 子进程验证，非原生窗口交互验收。
- 最新查询遇忙最多退避 5 次，累计 3.45 秒；覆盖 1.05 秒持续繁忙后恢复。已接入 make test / make test-gui 和 CI 配置；远端 CI 未运行。
- 真实 FSEvents A/B/A 的 9 类生命周期检查均通过，最终索引状态一致；排除目录写入后普通文件仍正常入库。
- HTTP 40 个同步请求中 4 个返回正确结果、36 个明确 503 背压；health/status 同时可用，随后搜索恢复 200，临时进程组与端口已回收。没有用 CLI 绕过背压。

## 对比方法

- 基线 a7e6d8a5b5f6cd58008db11ce2d6fc6a4638d1a8。已 fetch；本地领先 origin/main 5 个既有提交、behind=0，本轮制品不称远端主干发布。
- 真实索引通过只读 SQLite online backup 冻结：7,226,486 条、5,410,697,216 字节。每个进程使用独立 APFS 克隆副本，原副本不传给 Database::open。
- 相同 driver、release rlib、Rust -O/-C lto=fat、memory 后端、limit=300、固定时间；两轮串行 A/B/A，共 6 个独立进程、9 类查询。完整、有序结果逐字段序列化后比 SHA256。
- 进程 CPU 使用 getrusage，查询计时排除结果 JSON 编码；RSS 外部 libproc 采样，进程峰值包含加载和结果编码。阶段 RSS 增量仅作估计，不解释为精确分配量。
- 同一 Mac 128 GiB；保留用户既有运行应用与其它后台任务，不清空系统页缓存，加载时间只作暖缓存观测。CPU 时间与查询结果为主证据；不同 suite 正式测量顺序运行。
- watcher 使用 30 万条合成索引、独立真实目录，执行创建/修改/重命名/删除及 .git 目录写入，A/B/A 比较进程 CPU 与峰值；不把它当作真实 723 万条常驻 watcher 的替换验收。

## 性能结果

以下为同一冻结数据和相同负载的中位数。真实索引为两轮 A/B/A（4 个基线进程、2 个候选进程）。

| 场景 / 指标 | 优化前 | 优化后 | 变化 |
| --- | ---: | ---: | ---: |
| 723 万条真实索引，9 类查询累计 CPU 时间 | 22.556 秒 | 5.443 秒 | 降低 75.9% |
| 同一真实索引，code 查询 CPU 时间 | 4.543 秒 | 0.362 秒 | 降低 92.0% |
| 同一真实索引，无命中查询 CPU 时间 | 4.510 秒 | 2.083 秒 | 降低 53.8% |
| 同一真实索引，整组搜索进程 RSS 峰值 | 9.60 GiB | 3.72 GiB | 降低 61.3% |
| 同一真实索引，加载后 RSS | 3.71 GiB | 3.71 GiB | 基本不变 |
| 30 万条合成索引 + 真实目录更新/排除写入，watch CPU 时间 | 3.173 秒 | 0.017 秒 | 降低 99.5% |

真实索引所有完整、有序结果一致；10 万条合成 memory（3 轮 A/B/A）与 compact（1 轮 A/B/A）结果也一致。9 类查询包括广命中、普通词、无命中、glob、fuzzy、扩展名/排除过滤、时间过滤、Unicode 与路径搜索。CPU 时间是完成固定负载消耗的处理器时间，不能直接解释成日常 CPU 占用率下降同一百分比。

进程 RSS 峰值包括加载、搜索和结果编码。基线实测范围约 8.80–9.60 GiB，候选约 3.72 GiB；驻留页状态和分配器会造成波动。本轮未压缩常驻索引，加载后 RSS 的微小差异属于测量噪声。30 万条 watcher 是受控场景，不能声称已将用户当前 watcher 的 375 MiB 或日常占用直接降到该测试值。

主要汇总：[结构化比较](evidence/comparison.json)、[真实索引汇总](evidence/real-memory-summary.json)、[watcher 原始结果](evidence/watch-results.json)、[HTTP 验证摘要](evidence/service-summary.json)。冻结源码、二进制、原始完整结果与 RSS 采样保存在仓库忽略目录 `.build/performance-round1-20261017/`；其中真实索引副本含本机路径，仅供本地复核，不随文档共享。

复测源码标识为 `0.1.3-ga7e6d8a-dirty`，实际 SHA、源码文件指纹和证据位置见 [制品清单](evidence/artifact-manifest.json)。本轮没有正式发版，也没有用新候选替换当前 App。

## 保留的失败与排除项

- 首次 fetch、Python 系统缓存目录受沙箱限制；换授权网络/任务缓存后通过，不作为产品失败。
- 第一次 Rust 集成 51/52 通过，SQL 计划已正确但临时文件路径碰撞引发清理 NotFound；增加原子序号后最终 54/54 通过，保留原失败。
- GUI 扩充退避后首次测试遗留旧次数断言，恢复路径已成功但断言失败；修正断言并补耗时检查后完整回归通过。
- 第一版 watcher 基准把 DB 父目录覆盖到被监听目录，触发既有 DB 目录抑制，创建超时。修正为 state 与 root 同级后重新测；原目录保留 failure.json，不计产品回归。
- 初次真实搜索与 watcher 夹具准备短时重叠，真实搜索完整结果一致，但仅作探索；正式性能数据取后续顺序套件。

## 后续边界

本轮仍为全索引搜索；常驻 FileRecord/路径重复、全量 reload 的双份峰值、SQLite 既有候选截断、无界事件 channel、全扫描逐行 prepare/upsert 留待后续。HTTP cancel 不会终止已开始的 Rust 查询，由计算并发上限限制数量。

正式 QA、原生窗口与真实 HTTP 的全链路交互、用户实际后台 workload 的安装前后观察，以及远端 CI 均未运行。本轮证据是本地源码回归、真实冻结索引搜索与隔离的真实 FSEvents/HTTP 验证。

## 复跑入口

Rust 单测、严格 lint 和 GUI 回归：

```bash
CARGO_TARGET_DIR=/private/tmp/macevery-check cargo test --offline
CARGO_TARGET_DIR=/private/tmp/macevery-check cargo clippy --offline --all-targets -- -D warnings
make test-gui
```

测量工具为 tools/compare_round1.py、tools/perf_round1.rs、tools/watch_round1.py 和 tools/check_service_round1.py。均有 --help；查询工具分别链接两份冻结 rlib，watcher 工具分别启动两份冻结 CLI。输出目录必须使用新路径，避免覆盖既有证据。完整运行参数保存在每套测量 manifest/report 中。GUI 检查编译生产源码，测试开关仅替换依赖并禁用后台服务，不启动当前应用。
