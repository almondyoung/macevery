# MacEvery Top300一致性修复与紧凑副本验收

2026-10-20，r4。本机限定修复与原生验收保持；固定24小时窗口已按期结束并封存，含监听高CPU、GUI内存台阶及睡眠缺口，不作连续24小时无异常或无泄漏PASS，非正式发布或公司QA。

## 问题与改动

原CLI回退及Low Memory在limit=300时只对最多24,000条SQLite候选排序，Fastest对完整内存索引排序，相同输入可以返回不同Top300。本轮保留评分、完整结果字段及稳定顺序，改为流式评分和有界TopK。SQL预筛须覆盖所有可能匹配项，包含UTF-8边界、完整扩展名候选、basename/path必要字面量并集及多词分数界；Fastest查询热路径保持原实现。

完整搜索暴露了原索引物理布局的冷读成本。增加显式 `optimize --output NEW_DB --json`：只读源，经VACUUM INTO创建新文件，quick_check后用有界双cursor按ID顺序比较完整FileRecord及数量，独立核对内容标记和控制配置。拒绝既有输出、sidecar及活动事务。该命令不在启动时自动执行，也不自行替换真实库。

基线是已冻结的0.1.3-g8096e0a App，CLI SHA256为 `422e776c583ff9769c613c5f915f1b5c4e993da4085fa434042b81bd82a710ee`。本次最终受测CLI SHA256为 `836a9e678254776be807d0cd2c7c46e00021db25d9ab6900572e8ff46b5cbe99`。对应源码 `a4418a717811d036017efc028d02e036e9be97a4` 已推送并精确回读；已安装并固定App为0.1.3-ga4418a7；后续仅提交验收文档，不重编译或替换该固定制品。

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

物理整理只执行一次，随后两版本共用相同新布局。查询收益包含代码和物理布局条件变化，不能当作纯代码提升。整理及校验也会触页；复制新文件不等于清空系统缓存。本矩阵是一次顺序A/B机制验收，CLI CPU包含进程启动/初始化/编码，HTTP CPU包含刷新/编码，RSS采样可能漏峰值，生命周期HWM来自wait4。

## Fastest性能及受控FSEvents

[performance-gates.json](evidence/performance-gates.json)保存两块A/B/A、逐查询与全部原样本。常规9项、短词29项分别运行6个进程，每进程3轮，固定同一7,226,486条快照、limit=300及now。所有完整有序结果摘要一致；code和整组CPU普通中位与配对中位均在预设+5%门槛内。

| CPU项目 | 基线→候选普通中位，毫秒 | 普通变化 | 配对变化 |
|---|---:|---:|---:|
| 常规code | 156.912→155.641 | -0.810% | -0.889% |
| 常规broad | 47.678→48.888 | +2.538% | +2.164% |
| 常规nohit | 103.369→103.354 | -0.015% | +0.127% |
| 常规glob | 234.684→236.132 | +0.617% | +0.403% |
| 常规fuzzy | 485.366→480.991 | -0.901% | -0.903% |
| 常规filters | 34.100→34.265 | +0.484% | +1.136% |
| 常规mtime | 17.492→18.244 | +4.302% | +4.323% |
| 常规Unicode | 223.479→223.180 | -0.134% | -0.148% |
| 常规path | 169.105→171.981 | +1.700% | +1.490% |
| 常规整组 | 4,430.203→4,435.603 | +0.122% | -0.078% |
| 短词code | 152.288→152.379 | +0.059% | +0.178% |
| 短词整组 | 17,863.784→17,947.609 | +0.469% | +0.344% |

单查询按迭代池取中位，整组先按进程加和再取中位，不能直接相加逐项中位数。普通变化与配对变化分列，不择优替换。mtime的约4.3%上升保留；broad第一块CPU +5.490%、wall +5.412%，第二块分别-1.162%、-1.144%，不能把汇总PASS写成每一块均小于5%。

常规进程HWM中位增加679,936字节（约0.65MiB、+0.0426%），短词减少8,192字节；工程判断未观察到实质RSS增加，不称数学零增长。短词driver在6进程共528个load/查询释放后观测点的packed capacity全部为1,479,092,484字节。常规driver未埋capacity点，不能把528点归到常规套件。常规load wall普通+1.978%、配对+1.552%；短词load wall普通+3.551%、配对-0.432%，原波动均保留。

这是同输入库级Fastest比较，区别于上面的物理整理机制验收。缓存未清空，查询计时不含结果JSON编码，外部5ms RSS采样可漏短峰值；生命周期HWM包含加载、查询及编码。首个基线rlib命名导致的链接失败为NOT_MEASURED，原件保留，未混入样本。

受控watcher按A/B/A顺序各seed 300k条，创建、修改、重命名、目录创建/重命名/删除、文件删除、排除目录写风暴及排除后的正常更新九项通过，最终行集一致。候选CPU 11.876ms，对比两次基线中位12.351ms（-3.846%）；wall +0.329%，峰值RSS +24,576字节。每进程约9秒、CPU约12ms，属于事件正确性与粗粒度资源检查，不能证明长期idle CPU改善。

## Swift真实传输与候选制品

[Swift摘要](evidence/swift-real.json)绑定源码 `a4418a7`、生产Swift源码哈希及受测CLI哈希。七项全部PASS：加载中立即code、查询清空、快速替换只发布最新、warm code、真实传输超时后CLI成功、真实超时后取消CLI、最终清空。CLI与HTTP memory预检均为完整300条，六字段及原始顺序严格相同；cold请求在加载完成前发出，本次走HTTP，两个清空/替换场景的过期发布均为0。

真实超时等待分别15.050秒、15.054秒；成功回退返回300条，取消回退的owned搜索子进程以SIGTERM回收且发布0条。两个实际code HTTP响应也与oracle完整摘要一致。harness、wrapper、服务、全部oracle进程组及端口清理闭合，冻结输入未变。该证据使用生产Swift model与隔离真实HTTP/CLI，不代替已安装App窗口、系统权限或原生模式切换验收。

[源码回读](evidence/source-push-readback.json)证明远端main精确为 `a4418a717811d036017efc028d02e036e9be97a4`、behind=0且源码属于远端主干。[App制品](evidence/app-candidate.json)记录clean构建、strict/deep签名通过、实际Mach-O payload与受测CLI等价及源码archive冻结。签名后CLI SHA256为 `c254c4c307f7a975198f77a20fe63d20ff2dfec8823072645c8cd37fa472c8af`，与未签名受测CLI的差异已由规范化payload核对闭合；不以整文件哈希相同冒充验证。

## 完整搜索的查询成本与内存

两版本在相同紧凑快照上的一次机制对照，见[真实快照摘要](evidence/equivalence-compact-real.json)。以下是initial/code实际观测，非多轮中位或原生点击延迟；CLI包含启动与编码，HTTP包含服务搜索与编码。

| 路径 | wall基线→候选 | CPU基线→候选 | 峰值RSS基线→候选 |
|---|---:|---:|---:|
| CLI | 0.156→0.873秒 | 0.060→0.151秒 | 33.00→16.43MB |
| Low Memory HTTP | 0.092→0.758秒 | 0.028→0.108秒 | 33.34→17.01MB |

旧CLI/SQLite的该Top300与完整Fastest只有9条路径交集，不能把旧少做工作得到的低CPU/耗时当等价速度。候选完整搜索增加该次CPU与wall，同时工作集约减半；本矩阵所有候选查询仍通过原有15秒预算。稀有glob约0.76秒、无命中约1.97秒、多词code test约4.3秒；这些是已测输入边界，不保证任意复杂查询永不超时。

Fastest相同24阶段完整结果机制对照的采样footprint峰值1,591,527,080→1,590,986,456字节，RSS峰值1,595,523,072→1,594,949,632字节，近似持平。这是同输入机制证据；Fastest正式多轮CPU/RSS门禁以此前A/B/A为准。

## 实际采用与原生验收

[安装记录](evidence/installation.json)确认旧App经CUA正常退出，精确GUI/serve/watch启动身份均退出后，只读备份真实源；维护CLI逐条验证7,320,209条FileRecord，配置及内容ID/revision保持。主文件5,498,101,760→4,977,287,168字节；一次性CLI wall27.858秒、CPU13.307秒、HWM105,955,328字节。备份、quick_check、哈希与安装前后身份复查的总停用时间另计，不能把CLI维护耗时当完整切换时间。

原App、原真实DB及当时sidecar通过exclusive rename保留；已验证新副本和已测App才移到正式路径，每次rename后fsync目录，并保留逐步恢复图。源码、签名、实际payload及原配置回读闭合。旧冻结8096e0a、原67个观察样本、旧state、此前失败与回滚材料均保留；没有直接SQL修改真实files表，也没有改变roots/excludes、快捷键或系统权限。

[原生回执](evidence/native.json)记录CUA窗口验收：Fastest与Low Memory的code均显示300条；Fastest新建18字节、修改为35字节，Low Memory重命名后只显示beta新路径、删除后No Results。四步均经真实FSEvents更新并以只读精确路径SQL核对，临时文件与目录已清理。Low Memory服务实际参数为sqlite；切回Fastest后为memory，GUI/watch身份保持、serve按模式替换。窗口确认原Command-Shift-Space、原roots，权限从启动暂态Unknown回到Limited；未改TCC。

Fastest已恢复，code再次300条，快速替换只显示最新code，最后清空为No Query。窗口仅观察可见结果和总数；完整Top300六字段和原始顺序的一致性由固定快照矩阵承担。一次恢复Fastest点击未应用，重新读AX状态并重试确认Fastest=1后再关闭；清空后的footer可能保留上次300 results状态文案，但结果区为空，不把该文案当当前结果。

## 安装前后普通使用资源点

[点采样对照](evidence/live-resource-comparison.json)保留两次20秒No Query、Fastest的三个精确进程CPU、RSS和footprint；进程身份/电源状态在各自窗口内稳定。两点之间存在正常文件变化、原生图标展示及模式切换，不能据此建立同输入因果或泄漏结论。

| 三角色合计 | 安装前 | 安装后 | 变化 |
|---|---:|---:|---:|
| 单核平均CPU | 0.11662% | 0.09694% | -0.01968个百分点 |
| RSS | 1,809,727,488B | 1,811,709,952B | +0.110% |
| footprint | 1,447,693,600B | 1,703,791,928B | +17.690% |

本轮受控RSS和容量门禁通过，但普通使用点的footprint确实上升256,098,328字节，原因尚未建立；不以RSS持平概括全部内存指标，也不声称安装后内存整体降低。该差异已列为新24小时按进程身份/每小时趋势观察重点。

## 新24小时观察与保留边界

[启动回执](evidence/observation-start.json)：固定0.1.3-ga4418a7，窗口2026-10-19 19:45:23至2026-10-20 19:45:23（Asia/Shanghai），每60秒只读采样；第一样本App及DB身份匹配，collector PID、启动cookie、UUID与boot身份已核对。state为独立 `.build/topk-consistency-20261019/observation-24h`，既有观察工具哈希未变。

[交接健康回执](evidence/observation-health-at-handoff.json)进一步核对了8个新鲜样本、单一collector session、固定App/DB匹配及真实worker存活。首次严格核查发现初始回执UUID与运行时不同、sys.executable路径也不等于实际runtime，两个过严断言保留为校验拒绝；随后分别核对Python launcher和Python.app runtime的实际Mach-O LC_UUID，证明是同PID/start cookie/boot下的exec过渡。精确observer命令及稳定解释器路径已验证，原launch.local.json保留，另存launch-settled.local.json供后续UUID核查。没有重启collector、重置state或修改观察工具，窗口截止不变。

复用heartbeat `macevery-48`，名称更新为“MacEvery Top300修复版24小时观察”，ACTIVE，每小时只读跟进；正常无变化保持安静，仅异常、需用户操作或结束时通知。原ID封存旧观察时曾PAUSED，此后仅指向新窗口；旧collector仍已退出，旧67样本不合并。

观察期间不发测试搜索、不COUNT全表、不修改DB/权限、不重启或替换固定App，不静默延长或重开窗口。到期必须按实际样本、睡眠/缺口、进程身份段和每小时CPU/RSS/footprint分布、斜率与平台判断，并保存提交推送收尾记录；窗口到期本身不是连续24小时稳定或无泄漏PASS。

## 验收状态

| 项目 | 本稿状态 |
|---|---|
| Fastest code及短词CPU/RSS独立性能门禁 | PASS，保留逐项及分块波动 |
| 受控FSEvents九项 | PASS，长期idle另验 |
| Swift真实HTTP/CLI回退、15秒超时及取消 | PASS，原生App另验 |
| 源码提交/推送及精确SHA回读 | PASS，a4418a7 |
| 候选App构建、签名、payload及源码archive | PASS，0.1.3-ga4418a7 |
| 安装与紧凑DB adopt | PASS，原件与恢复图保留 |
| Fastest/Low Memory原生窗口、文件更新与资源回读 | PASS，footprint点采样上升待持续观察 |
| 新24小时固定版本观察 | SEALED_WITH_ANOMALIES_AND_SLEEP_GAPS，1426点，按实际覆盖收尾；非无泄漏PASS |

原48小时窗口已因本轮新任务停止，67个旧样本及state封存，不能拼入新窗口或称长期通过。原App与真实索引回滚材料均已保留；本次为本机限定验收，不代替长期稳定性、正式QA或发行版发布。

## 24小时观察封存（2026-10-20）

[结束摘要与逐小时同身份指标](evidence/observation-finish.json)。原窗口2026-10-19 19:45:23至20日19:45:23，collector于19:45:23.563以window_elapsed/exit0结束；实际PID已不存在、writer锁可独占、原文件哈希稳定。33个原始文件（manifest/state/journal/两份启动回执/逐小时累计材料等）及53份阶段核查材料已逐字复制并核验，本机受限原件与封存副本均保留。观察工具、App、真实DB身份未变，旧67样本没有拼入。

1426个端点，名义1441槽，少15槽；每角色1421个有效CPU区间，覆盖85177.990530秒（23小时39分37.991秒）。18:16至18:37的4个睡眠/电源/缺口区间排除1221.948779秒，其中清醒194.529306秒；睡眠时钟差累计约1027.420075秒。power_transition1、sleep_observed4、sampling_gap2，无capture barrier。最后端点超出名义截止约0.021538秒已原样记录，不静默裁成整24小时或补样。

| 同一进程身份 | 有效区间平均／最高分钟单核CPU | RSS首→末〔离散峰值〕MiB | footprint首→末〔离散峰值〕MiB |
|---|---:|---:|---:|
| GUI | 0.31788%／12.78158% | 150.422→149.422〔189.125〕 | 56.251→66.032〔89.485〕 |
| serve | 0.003101%／3.31636% | 1547.344→6.906〔1548.281〕 | 1543.564→1544.502〔1544.517〕 |
| watch | 0.54003%／85.63487% | 19.688→23.000〔103.078〕 | 6.251→10.813〔92.204〕 |

异常线与未覆盖项：

- watch在10:59至11:07端点共9个分钟区间超过50%，合计419.515 CPU秒；11:03峰值85.635%，11:07 RSS/footprint峰值103.078/92.204 MiB。11:08 revision一次增加7,341,204，last_indexed_at前进且内存回落。之后498个**可测**区间无≥50%复发，最高16:46为3.0693%；不包含睡眠失效区间。13:03 footprint升至14.329 MiB，14:38回10.829，最终10.813，没有后半段持续爬升形态，但未排除更慢泄漏。
- GUI在10:21、15:31、16:47多次阶跃，随后分别回落；16:47高位持续约45分钟，17:32回73.173 MiB。19:20再次上升，19:21及19:24达全窗footprint峰值89.485 MiB，19:25回72.860、19:42再降66.001，最终66.032。不能称全程平台，也不能仅凭阶跃和回落确定泄漏或根因。
- serve末段RSS降至6.906 MiB，footprint仍1544.502 MiB；没有证据证明索引已释放，不能把RSS变化当作整体内存减少。
- 夜间多数小时低CPU、内存接近平台；10:45至11:45 watch平均CPU12.026%，11:45至12:45 GUI平均2.415%，异常不可被整窗低均值掩盖。逐小时分位、OLS斜率和端点随摘要保留；离散峰值可能漏采样间峰值，短命CLI CPU也可能漏记。
- 普通安装前后点采样footprint **+17.690%** 仍未解释，独立于受控同输入A/B证据；不能据RSS下降声称内存整体降低。原始触发path/flags未持久记录，不能断言11点事件必为某种flag或该次全扫已被证明不必要。

固定源码可确认：MustScanSubDirs等恢复位当前都会丢弃触发路径并升级为全部配置roots；普通目录事件仅递归该子树，无运行十五小时后定时全扫逻辑。扫描每条记录更新公开indexed_at并触发内容日志，可能放大revision/保留日志清理与后续reload；这些是源码机制及风险，不是本次具体flag的回溯证明。下一轮先补有界触发日志、按恢复语义收紧范围，再减少可避免写入及prepare成本，同时排查GUI。indexed_at的公开语义与代际清理必须保持或明确兼容，不通过删除触发条件伪造低revision。

下一轮入口：[监听恢复、写入与GUI资源优化状态](../watch-recovery-20261020/STATUS.md)。本窗口已结束，不自动重新观察或拼接数据；现有heartbeat接续已授权后续工作。历史大临时库将按本次明确清理授权在先保留必要非大库证据后删除，原始24小时及旧67点、冻结App和真实回滚材料始终保留；被删测试输入不再宣称仍可逐字复跑，清理结果另记。报告前文关于保留所有隔离大测试库的叙述是原验收时状态，后续删除以清理清单为准；观察结束后新轮替换App另按门禁验收。
