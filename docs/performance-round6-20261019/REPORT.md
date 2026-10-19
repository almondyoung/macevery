# MacEvery 第六轮：加载 CPU 与启动就绪优化

2026-10-19，r1。受控正确性、性能和更新门禁通过；App 替换、原生窗口验收和最终资源观察待执行。

## 范围与改动

基线主干为 `26a7246`，生产代码与运行 App `0.1.3-gd701658` 一致。复用上一轮已测 Release 制品前核对全部源码及二进制哈希，并保留独立基线源码、制品和原 App 回滚副本。

本轮只改加载与 watcher 启动路径：

- `compact_index.rs` 把 lower 准备与实际布局分开，常见记录只计算一次 suffix 共享布局；用 checked 长度上界跳过重复计算，超过边界仍调用原精确校验，保留超大 alias 的接受范围及增量 preflight 契约。
- 仅构建器缓存最近成功的前缀 ID；完整目录字节相等才复用，未命中仍走原有有界字典。该缓存随构建器释放，不增加常驻容量，delta 不使用提示。
- 标准五种 `FileKind` 直接解析；所有大小写、别名、Unicode 和未知值继续原 fallback。
- SQLite 文本读取省去一次冗余 type 调用，保留 NULL 指针检查、text→bytes 顺序、显式字节长度与 UTF-8 验证；数值读取的严格类型/解析分支未改。依据 [SQLite 官方列读取 API](https://www.sqlite.org/c3ref/column_blob.html)。
- watcher 通过两条小查询读取 roots/excludes，跳过不使用的全库分类计数；checkpoint、扫描、重试、增量更新及清理规则不变。

查询扫描、短词 matcher、score/TopK 排序、56B 热记录、元数据编码、字符串存储和 64MiB 前缀池准入规则保持。GUI 启动顺序与交互代码未改。

## 正确性与门禁

Debug/Release 各 123 项单测、420 次事务全字段/score/有序结果差分、定向 prefix 更新集成测试通过；格式、严格 all-target Clippy、Python 时间过滤 guard 和 GUI 取消/限流/子进程回收检查通过。新增测试覆盖标准值全部大小写/别名、Unicode/NUL、SQLite NULL/空/NUL/非法 UTF-8/BLOB/数字、严格 i64，以及 roots/excludes 顺序与无 files 表依赖。缓存开关两路的完整字段、arena 字节、字典及容量一致。多 GiB alias 未实际分配，保留原精确校验并检查长度上界与溢出边界。

复用同一个 7,226,486 条冻结快照，SHA256 `68bd22081af1eb5ec9164037407cb6c9c13349a7f8e460fa9aa682b3286b5b29`。原九类与 29 项短词各两组 A/B/A、6 个独立进程，双方 compact，limit=300、fixed-now=1792195200、query-rounds=1、5ms 采样。每次返回的完整有序 FileRecord/score 哈希一致；真实大库最多 300 条，完整边界由夹具承担，不扩大为所有可能查询的证明。

预设短词组、fresh/hot code 配对 CPU 中位最多 +5%，任一词两块都超过 +10% 且 +5ms 则调查；容量最多 +1%，主 RSS 指标增加超过 5% 则调查。实际 fresh code 配对 -1.13%、hot code -3.50%、短词组 -1.81%，无重复明显回退，全部门禁通过。

## CPU、内存与启动对比

下表为同一负载位置的普通中位数；配对变化与全部原始样本单独保存。加载段包含 Database::open/init 和 packed 加载，不称纯 CompactIndex::load。容量、RSS 和 footprint 各自计量。

| 指标 | 第五轮基线 | 第六轮 | 变化 |
| --- | ---: | ---: | ---: |
| 九类组加载 CPU（秒） | 5.553 | 5.011 | -9.76% |
| 九类组加载时间（秒） | 14.509 | 13.849 | -4.55% |
| 短词组加载 CPU（秒） | 5.420 | 4.776 | -11.88% |
| 短词组加载时间（秒） | 14.362 | 13.495 | -6.04% |
| 九类组加载后 RSS（GiB） | 1.130 | 1.130 | -0.00% |
| 九类组生命周期峰值 RSS（GiB） | 1.537 | 1.537 | -0.01% |
| 九类首次 code CPU（ms） | 198.06 | 201.12 | +1.54% |
| 九类累计查询 CPU（秒） | 1.776 | 1.820 | +2.46% |
| 29 项累计查询 CPU（秒） | 6.590 | 6.433 | -2.39% |
| 短词组热 code CPU（ms） | 170.53 | 163.53 | -4.11% |
| 状态读取至后台 ready（秒） | 19.764 | 18.820 | -4.78% |
| 状态读取至后台 ready CPU（秒） | 5.532 | 5.117 | -7.51% |
| 状态读取至首次 code 完成（秒） | 19.974 | 18.995 | -4.90% |

容量精确为两版均 1,537,288,452B，八项明细完全一致：记录407,371,776B、元数据176,194,432B、arena889,192,448B、扩展38,014B、其他容器150,128B、前缀 payload51,758,742B、条目8,388,608B、查找表4,194,304B；468,666 个前缀、157,914 个 escape、111 个 narrow block，64MiB 池限制保留。上一轮的内存收益维持，RSS 基本持平。

保留回退：九类首次 code 普通中位 +1.54%（+3.06ms），九类总查询 +2.46%；九类长 nohit +6.99%（+7.94ms），短词组所附长 nohit +7.36%（+8.04ms）。两组 code 位于九类第2项和短词第10项，不能跨位置比较；未宣称所有查询提速。

新 `tools/check_startup_round6.py` 在计时外准备迁移副本，逐次独立 clone，随机认证 loopback 端口。测量 `status CLI → serve memory → /status ready=true && stale=false → 首次 code`；每次总数一致、首次300条完整六字段 HTTP 数组含 score/顺序哈希一致。CPU 采用 macOS libproc/Mach timebase，status/退出采用 wait4；抽查既有末次累计27.657934s与 wait4 27.683554s相符（退出多25.620ms）。复制、准备和控制器 CPU 排除。

初始两块 ready wall 按运行顺序配对 +1.765%/-5.344%，因此只补一整块 A/B/A并保留原6次；合并9次，第三块 -4.201%，配对中位 -4.201%。ready 累计 CPU 配对中位 -7.512%；至首次 code 时间配对 -4.153%，累计 CPU 配对 -7.254%。其中一块墙钟仍为正，改善幅度有限。服务 lifetime HWM 普通中位1.536→1.536GiB，基本不变。status 退出附近最后一次采样不代表准确退出 RSS，不能把该采样增幅当作进程内存翻倍；主要看 wait4 HWM。

该启动路径近似当前 Swift 的状态读取 gate，未启动第二个原生 App，不包含 Swift/真实窗口首帧、快捷键、watcher 或加载中抢先输入。后台 ready 不能用 bind 日志或单独 health200替代。所有测试进程组、端口与临时副本清理通过，源快照未变。缓存未清空，用户其他应用保持运行，墙钟和个别查询存在波动；没有选择或删除不利样本。

## 更新与恢复

复用 mixed-case prefix HTTP oracle，两版13阶段的完整 HTTP 返回哈希相等。三波每波更新1,000普通行并混合探针增删改名/同ID替换/主键变化，full reload=0、incremental applies=1/2/3；每波另查一个普通行，不把它称为全1,000行的HTTP字段证明。status-only观察 stale不触发刷新。显式 reload、4,097事件超限、历史断档/缺表、读取失败503后恢复均通过。

40并发两版均2个正确200、38个明确busy503；health/status与请求重叠且成功。单次固定A→B生命周期CPU27.684→24.582秒（-11.20%），但其中首次load CPU5.415→5.534秒（+2.2%），均保留；此生命周期混合多次完整重载/故障恢复，不替代启动A/B/A。HWM约1.539→1.538GiB，基本持平。

30万条受控索引真实FSEvents A/B/A，九类创建/修改/改名/目录生命周期/排除目录噪声及后续恢复全部通过，最终状态一致。watch CPU A=0.018167/0.020088秒、B=0.011617秒；峰值A约11.20/11.22MiB、B约7.84MiB。该小负载单块只作有界观察，不作长期CPU/内存承诺。

## App 与本机验收

真实索引只读在线备份已完成：7,310,058条、quick_check=ok，SHA256 `dd4db6f36f1b059d71a57ad10752f399de722fd92721fa19e301d892a6c2ced8`；数据库ID及roots/excludes已保存。

待执行：提交推送已测源码并精确回读；干净构建、签名及归一化 payload 检查；原位替换；实际窗口 create/modify/rename/delete 和 code300 验收；清空查询、最终20秒资源观察、配置与索引身份回读。

旧 App 的原生设置及空查询已核实，替换前20.073秒 CPU合计0.216%一个核、RSSsum1.734GiB，修订增加5；该点观察保留普通后台活动，不能替代固定输入基准或长时泄漏验证。回滚 `.build/MacEvery.rollback-20261019-before-round6.app` 已保留。未直接编辑/重建真实索引、修改roots/excludes/模式/快捷键或扩大系统权限。

## 证据

[检查](evidence/checks.json)、[九类查询](evidence/search-summary.json)、[短词](evidence/short-summary.json)、[性能门禁](evidence/performance-gates.json)、[容量](evidence/index-memory.json)、[启动初始](evidence/startup-initial.json)、[补一块启动](evidence/startup-confirm.json)、[全部九次启动](evidence/startup-combined.json)、[HTTP更新恢复](evidence/service-summary.json)、[FSEvents](evidence/watch-results.json)、[受测制品](evidence/candidate-artifacts.json)。

原始源码/制品、完整真实结果、资源样本与所有回滚资料保存在忽略目录 `.build/performance-round6-20261019/`，不推送真实索引及完整结果。原位/native验收完成后补独立文档提交；不因文档提交重建App。远端CI、正式QA、长时间后台及受保护目录扩展覆盖未执行。
