# MacEvery 第七轮：记录容量与启动路径优化

2026-10-19，r1 受控验收稿。正确性、查询/内存、最终启动时序、HTTP 更新恢复和 FSEvents 门禁通过；App 替换、本机窗口及 Git 结果仍为 **PENDING**。本稿是工程验证记录，不是正式 QA 或发布完成声明。

## 范围与方案

主干基线 `a613164`，替换前 App 源码 `2f32302`（`0.1.3-g2f32302`）。同一份 7,226,486 条冻结快照 SHA256 为 `68bd22081af1eb5ec9164037407cb6c9c13349a7f8e460fa9aa682b3286b5b29`。不改搜索偏好、roots/excludes、快捷键或系统权限，不手动重建或直接修改真实索引。

- **热记录 56B → 48B。** 保留 basename/basename_lower 完整 u32 长度，通过既有 suffix 布局的父字段标记推导起点，省去两个重复 start。实际搜索字符串仍连续存储；独立 basename/lower、Unicode/NUL、共享原路径前缀及原样 fallback 保留。元数据六字段、NULL/i64 极值、前缀池准入、arena、增量预检、墓碑复用和碎片限制不变。
- **轻量 `bootstrap --json`。** 保留 `Database::open/init` 及迁移，单读事务内以 `EXISTS … LIMIT 1` 判断记录存在性，再读取 roots/excludes 和 last_indexed_at。JSON 字段固定为 db_path、has_records、roots、excludes、last_indexed_at。读取/提交失败回滚并报错，不伪造空库；原 `status` 完整计数及语义保留。
- **严格就绪后完整统计。** GUI 先按 records/roots 四格处理首次索引及服务/watcher 启动；有记录时等待 `ready=true && stale=false` 再启动 full status，期间仍可查询。生产固定 60 秒截止预算，反复轮询和模式重启继承剩余预算；服务启动失败、退出或超时转完整统计 fallback，慢加载下可能重新出现重叠。显式 Refresh 消费延后请求并立即统计。统计失败保留成功 bootstrap 的可用性并显示错误，不伪造数量。
- **过期结果隔离。** bootstrap/full status 捕获 index-read generation；重建开始及完成时更新 generation，旧 metadata 即使晚于重建完成返回，也不能覆盖新状态、配置或重复触发首次索引。查询取消、限流和自有子进程清理规则保留。

短词 matcher、scorer/TopK、SQLite 全量投影、快照发布、内容 revision 及有界 journal 未改。超限、历史缺口、缺表、数据库替换和读取失败的回退/恢复契约保留。首版启动时序被拒绝后，只改 GUI 调度与启动 driver；Rust 源码及 Release CLI/library 哈希未变，查询、容量、HTTP/FSEvents 证据可继续使用，见[制品与复用回执](evidence/candidate-artifacts.json)。

## 正确性与门禁

Debug/Release 各 **129 项单测**、**3 项 bootstrap CLI 集成**、**420 次事务全字段/score/有序结果差分**及 prefix 更新集成通过；fmt、严格 all-target Clippy、Release build 通过。布局测试覆盖父字段分支、独立 basename/lower、共享前缀与 raw fallback、Unicode/NUL、完整长度边界、metadata-only arena 不增长及删除/复活。多 GiB 字符串仅作边界计算测试，未实际分配。

bootstrap 验证空/非空记录 × 空/非空 roots 四格、旧库迁移/重开 schema 稳定、配置顺序、时间缺失/非法文本/i64 极值、WAL 同快照，以及 SQL/损坏错误不伪空、失败事务释放和嵌套 BEGIN 不动调用者事务。CLI 检查精确 JSON/转义和非法参数不打开数据库，原 status 计数 schema 保留。

最终 GUI 新旧回归全部通过：strict ready 前不统计，固定 60 秒预算及模式重启不延长，启动失败/退出/超时 fallback，显式 Refresh、延迟精确计数、四格/单次首次索引，以及旧 bootstrap/full status 晚于重建完成返回的 generation guard。GUI 检查使用私有 fake CLI 和 URLProtocol，不代表真实窗口/本地服务交互；本机验收另列 PENDING。证据见[检查与最终 GUI 回执](evidence/checks.json)。

## 容量、查询与内存

容量 **1,537,288,452B → 1,479,092,484B**，减少 **58,195,968B**；记录容量 407,371,776B → 349,175,808B，其余分项完全一致。7,226,486 条记录、468,666 个前缀、157,914 个 metadata escape 与 111 个 narrow block 均相同。容量不等同 RSS/footprint/HWM，见[八项容量](evidence/index-memory.json)。

原九类、29 项短词各两组 A/B/A，双方 compact，limit=300、fixed-now=1792195200、query-rounds=1、5ms 采样。完整有序 FileRecord/score 摘要全部相等，所有样本保留。下表为同一负载位置的普通中位数，配对门槛单独计算。

| 指标 | 基线 | 候选 | 变化 |
| --- | ---: | ---: | ---: |
| 九类加载时间（秒） | 13.621 | 14.055 | +3.18% |
| 九类加载 CPU（秒） | 4.969 | 5.146 | +3.56% |
| 短词组加载时间（秒） | 14.022 | 13.279 | -5.30% |
| 短词组加载 CPU（秒） | 5.134 | 4.763 | -7.21% |
| 九类加载后 RSS（GiB） | 1.132 | 1.080 | -4.54% |
| 九类生命周期峰值 RSS（GiB） | 1.538 | 1.487 | -3.35% |
| 短词加载后 RSS（GiB） | 1.130 | 1.080 | -4.45% |
| 短词生命周期峰值 RSS（GiB） | 1.538 | 1.487 | -3.28% |
| 九类首次 code CPU（ms） | 173.836 | 180.277 | +3.70% |
| 短词组热 code CPU（ms） | 152.269 | 154.937 | +1.75% |
| 九类累计查询 CPU（秒） | 1.505 | 1.508 | +0.22% |
| 29 项累计查询 CPU（秒） | 6.024 | 6.039 | +0.25% |

配对 CPU 中位：fresh code **+3.502%**、hot code **+1.629%**、短词组 **+0.330%**，均低于预设 +5%；没有词在两块均超过 +10% 且 +5ms。两种 code 位于不同负载位置，不能互比。保留首次 code 普通中位 +3.70%（+6.44ms）、热 code +1.75%（+2.67ms）；两组加载 CPU 方向不同，不能称全部 CPU 下降。加载含 Database::open/init，RSS、footprint 与容量分别计量，未清系统缓存。见[九类](evidence/search-summary.json)、[短词](evidence/short-summary.json)、[全部门禁](evidence/performance-gates.json)。

## 最终启动时序

[启动 driver](../../tools/check_startup_round7.py) 基线为 `status → serve memory`，最终候选为 `bootstrap → serve → strict ready → full status 与首次 code`。两组 A/B/A 共六次独立进程；ready 必须 `ready=true && stale=false` 且加载数与计时外精确 oracle 一致，首次查询另验 300 条完整六字段 HTTP 数组及顺序摘要。各次配置/数量/有序摘要均一致，进程组、端口和临时副本清理通过，源快照未变。见[最终六次启动](evidence/startup-summary.json)。

| 指标（从流程开始计） | 基线普通中位 | 候选普通中位 | 变化 |
| --- | ---: | ---: | ---: |
| 至 strict ready（秒） | 18.528864 | 13.661558 | -26.269% |
| 至 strict ready 累计 CPU（秒） | 5.450307 | 5.108019 | -6.280% |
| 至首次 code 完成（秒） | 18.754934 | 13.873515 | -26.027% |
| 精确 full status 完成（秒） | 2.091408 | 14.978217 | 更晚显示数量 |
| 首次查询及完整统计均完成（秒） | 18.754934 | 14.978217 | -20.137% |
| 完整生命周期 CPU（秒） | 5.703221 | 5.628264 | -1.314% |

两块 ready wall 配对 **-22.378% / -27.716%**，配对中位 **-25.047%**；ready CPU 配对中位 **-7.843%**。完整生命周期 CPU 配对中位 **-2.995%**，两块分别 **+0.061% / -6.051%**，不能称各块都下降。精确数量显示从约 2.091 秒延后至 14.978 秒，这是换取提前可查询的代价，统计并未消失。

ready/首次查询累计 CPU 包含 bootstrap、该时点 full status 已耗 CPU 和服务 CPU；候选 ready 时 full status 尚未启动，最终生命周期仍计入其完整 CPU及服务关闭。wait4 用于退出 CPU/HWM，运行中 CPU/RSS/footprint 用 libproc/Mach timebase；各进程 HWM 单列，不能相加称为同时峰值。迁移/oracle/复制及控制器 CPU 排除，采样和退出观察有轮询/调度误差。该测量不启动 watcher 或窗口，不证明原生首帧、用户抢先输入、超时 fallback 或长期资源；GUI 状态机由前述回归另验。

### 保留的首版失败

首版在 bootstrap 后让完整统计与加载并行：初始两块 ready wall 配对 **+4.374% / -10.397%**；为澄清分散只补一整块 A/B/A，得到 **+0.866%**。合并九次配对中位 **+0.866%**，未通过“必须改善”门槛；普通中位虽约 -4.8%，不能覆盖门禁失败，因此未安装该时序。

未将失败归因为 I/O 争用；延后统计只改变可验证的执行时机。原九次不并入最终六次，也不删除，原工具、Swift 源码、哈希和原始记录均保留。见[初始六次](evidence/startup-rejected-initial.json)、[补一块](evidence/startup-rejected-confirm.json)、[拒绝版九次合并](evidence/startup-rejected-combined.json)。

## 更新与恢复

[HTTP 两版结果](evidence/service-summary.json) 全部阶段一致。三波每波更新 1,000 普通行及 mixed-case 共享前缀探针，full reload=0、incremental applies=1/2/3；每波抽查一个普通行，不扩大为全 1,000 行 HTTP 字段证明。40 并发均 2 个正确 200、38 个 busy503；显式 reload、4,097 事件超限、历史断档/缺表、读取失败 503 后恢复通过。单次固定 A→B 生命周期 CPU **25.202→26.113 秒（+3.62%）**，首次 load CPU **4.946→5.207 秒（约 +5.27%）**；HWM 约 1.538→1.486GiB。该混合多次重载/恢复的工作流不代替启动 A/B/A。

[真实 FSEvents A/B/A](evidence/watch-results.json) 使用 30 万条受控索引，创建、修改、改名、目录生命周期、排除目录写入噪声及后续更新均通过，最终状态一致。watch CPU A=0.020918/0.018591 秒、B=0.017341 秒，HWM 约 7.86→7.81MiB；仅作有界观察，不作长期资源承诺。自有进程组、端口和临时副本已清理，源快照未改。

## App、本机与 Git：PENDING

App 构建身份/签名/原位替换、真实窗口查询与文件生命周期、设置/权限/索引身份回读、安装后资源观察均为 **PENDING**。源码提交/推送/精确回读、最终文档提交及干净状态回读同样 **PENDING**；保留旧 App 回滚资料，不因文档更新重建 App，不自动增版本或建 tag。

全部公开证据见本轮 [evidence](evidence/)。原始源码/制品、真实完整结果和全部失败保留在本地隔离证据目录；公开摘要仅保留计数、哈希、数值和相对引用，不发布真实索引、普通路径或 token。不同实时库状态/进程历史的资源点观察不能替代同输入比较。远端 CI、正式 QA、长时间后台与受保护目录扩展覆盖均未执行；受控门禁通过不等于已部署或完成本机验收。
