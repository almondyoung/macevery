# MacEvery Top300一致性修复

更新：2026-10-20，r4，Asia/Shanghai。本机修复/采用/原生验收保持；固定App 0.1.3-ga4418a7的24小时窗口已封存，状态SEALED_WITH_ANOMALIES_AND_SLEEP_GAPS，非连续24小时无异常或无泄漏PASS。

## 任务与边界

- 本机代码/验证/部署三条工作线；主仓库 `macevery`，基线主干46a6078，旧基线/冻结App源码8096e0a，新运行及固定App源码a4418a7，版本0.1.3-ga4418a7。
- 目标：CLI回退、Low Memory、Fastest在相同快照/查询/limit下的Top300全部字段与顺序一致，索引增改名删正确；有界内存，不牺牲Fastest短词热路径。比较CPU、RSS/footprint、耗时、真实15秒超时和取消。
- 允许修改SQLite搜索/流式TopK及有针对性的测试和验收工具；不改变roots/excludes、快捷键或系统权限；可临时切换模式验收后恢复Fastest。只允许经全量验证、保留原件的物理紧凑副本替换，不直接SQL写真实files数据，不删除冻结App和旧原始证据。
- 固定基线 `.build/stabilization-20261019-8096e0a/frozen/MacEvery.app`、源码archive、原48小时观察state全部保留；旧观察不足48小时，不算长期通过。修复版单独新建24小时state，禁止拼接窗口。
- 安装前必须通过同快照三路径完整结果对照、增量/真实FSEvents、Fastest短词CPU/RSS对照及Swift真实15秒超时/取消；保留失败，不能用候选截断维持表面速度。

## 执行计划

1. 封存旧观察并暂停原heartbeat；保存基线制品、测试输入与资源端点。
2. 实现完整候选流式评分和有界TopK，复用匹配/排序语义；定向覆盖旧mtime、Unicode/glob/fuzzy、过滤、NULL与边界limit。
3. debug/release回归与静态检查；顺序运行真实隔离索引正确性、更新、CPU/内存/超时对照；Fastest code及短词预设CPU回退门槛5%，RSS无实质增长。
4. 提交推送修复源码并回读主干SHA；构建独立候选App，核对源码、签名与制品哈希。验证通过才替换运行App，保留原回滚。
5. 原生搜索与文件更新、最终资源点采样；保存并提交推送验收记录，为新固定版本重新开始24小时只读观察和每小时安静跟进。

## 当前状态

- 搜索代码：完成完整流式TopK、UTF8 guard、完整ext候选、basename/path必要literal并集、同snapshot和严格分数界（含多词），Fastest热路径未改。独立源码review无阻断。
- 最终Rust验证：Debug/Release各149单测、各3项bootstrap CLI与3项optimize CLI、420事务差分及prefix集成全部通过；严格all-target Clippy、格式和release build通过。受测CLI SHA256为 `836a9e678254776be807d0cd2c7c46e00021db25d9ab6900572e8ff46b5cbe99`，源码 `a4418a717811d036017efc028d02e036e9be97a4` 已推送并精确回读。见[最终检查](evidence/staging-checks.json)、[源码回读](evidence/source-push-readback.json)。
- 正式原布局：**REJECTED_COLD_IO**。7,226,486条原快照，候选memory/sqlite各24phase全部正确且小于15秒；CLI rare glob在独立新副本15.014秒超时。SQLite同glob14.532/nohit14.857秒仅接近边界。完整失败保留，禁止用热缓存或延长查询预算过关。
- 已定位：同列新dense索引冷读12.719→0.591秒；仅REINDEX仍11.650秒，37.08秒/Python114MB峰值，未采纳。VACUUM隔离副本55.746秒/Python201MB峰值，主文件5,410,697,216→4,896,702,464字节，ID/revision同；fresh clone原名索引扫描0.590秒。这是一次性物理整理收益，非仅搜索代码或冷缓存清空收益。
- 维护API：显式 `optimize --output NEW_DB --json` 已完成并验收。只读源经VACUUM INTO生成新文件，quick_check及有界双cursor核对完整FileRecord（含ID、顺序和数量），保留control pin；拒绝既有输出、sidecar和活动事务。Driver只在显式 `--prepare-compact` 时运行，成功才把同一个新母库供两版本6views使用；不自动startup或替换真实库。
- 最终隔离矩阵：30k及7,226,486条快照均6views×24phase完成；候选CLI、Low Memory、Fastest的72项完整六字段和顺序全部等于旧Fastest全量oracle，候选全部在15秒内，增改名删计数及清理门禁通过。旧CLI/sqlite仍有各自历史TopK差异，未作为候选豁免。见[30k摘要](evidence/equivalence-compact-30k.json)、[真实快照摘要](evidence/equivalence-compact-real.json)。
- 整理保真与成本：真实库逐行验证7,226,486条；源副本SHA前后相同，ID/revision、roots/excludes/meta保持一致。压缩前后旧memory全部24项有序摘要一致。一次性CLI维护28.087秒、CPU13.852秒、进程HWM101.02MiB；含driver核验共34.229秒，文件5,410,697,216→4,896,702,464字节。该成本独立于查询计时，收益包含物理布局变化，不能称仅代码提升或冷缓存清空。原始、失败和紧凑副本均保留。见[原布局对照](evidence/original-vs-compact-memory.json)与[验收报告](REPORT.md)。
- 性能：常规9项与短词29项各两块A/B/A、所有完整有序摘要一致；code及整组CPU门槛通过。常规code CPU普通-0.810%/配对-0.889%，短词code +0.059%/+0.178%，短词整组+0.469%/+0.344%。mtime约+4.3%、broad首块CPU+5.490%完整保留。短词528个capacity点全部1,479,092,484字节；常规HWM +679,936字节，短词-8,192字节，工程判断无实质增长。见[性能证据](evidence/performance-gates.json)。
- 受控FSEvents：300k数据A/B/A九项创建/修改/改名/删除/排除风暴及恢复全部通过，最终行集一致。仅约9秒/进程的事件与粗粒度资源检查，不等于长期idle通过。
- Swift：七项真实HTTP/CLI集成PASS，exact Top300硬门禁、加载中查询、清空/快速替换、真实15.050/15.054秒超时回退及取消、owned清理均闭合；输入未变。见[Swift证据](evidence/swift-real.json)，原生App窗口与权限另由下述回执闭合。
- 候选App：从clean `a4418a7` 构建0.1.3-ga4418a7，strict/deep签名、实际payload等价与源码archive冻结通过；签名后CLI SHA为 `c254c4c307f7a975198f77a20fe63d20ff2dfec8823072645c8cd37fa472c8af`。见[App制品](evidence/app-candidate.json)。
- 旧观察：**PRESERVED**。已核对启动cookie/UUID/boot停止collector，67个原样本及旧state保留，封存时原heartbeat PAUSED；随后同ID仅复用作新24小时，旧collector不再运行；旧窗口未完成。
- 安装/原生：**PASS**。真实源停用只读备份，完整核对7,320,209条后adopt等价紧凑副本与已测App；原DB/App/sidecar及恢复图保留。Fastest、Low Memory原生code各300条，18→35字节修改、改名/删除经窗口与只读SQL核对；fixture已清理，恢复Fastest/memory及No Query，原快捷键/配置/权限Limited保持。见[安装](evidence/installation.json)、[原生](evidence/native.json)。
- 最终资源：普通使用20秒No Query三角色单核CPU0.11662%→0.09694%，RSS +0.110%，footprint +17.690%（+256,098,328字节）。不是同输入受控A/B，footprint差异原因尚未建立；不宣称内存整体降低。受控Fastest容量/RSS保持，点采样footprint为新24小时观察重点。见[资源对照](evidence/live-resource-comparison.json)。
- 新24小时：**SEALED_WITH_ANOMALIES_AND_SLEEP_GAPS**。2026-10-19 19:45:23至10月20日19:45:23（Asia/Shanghai），每60秒采样；初始worker启动cookie/UUID/boot与App/DB身份已核对，工具未变。heartbeat `macevery-48` 已更新名称和目标为本窗口、ACTIVE，正常保持安静；旧窗口与旧67样本独立保留。见[启动](evidence/observation-start.json)。

## 观察入口与下一步

- 固定App：`.build/MacEvery.app`，源码 `a4418a717811d036017efc028d02e036e9be97a4`；冻结副本及源码archive：`.build/topk-consistency-20261019/frozen`。后续验收文档提交不重建App。
- 新state：`.build/topk-consistency-20261019/observation-24h`，优先读manifest.local.json/state.json/launch.local.json，再运行 `python3 tools/observe_app_resources.py summarize --state /Users/yangtao/pine/code/macevery/.build/topk-consistency-20261019/observation-24h`。
- worker存活必须核对PID/start cookie/boot；UUID及实际解释器路径对照launch-settled.local.json，不只信running字段。原launch捕获了Python launcher的exec前UUID，与稳定Python.app不同；两个实际Mach-O LC_UUID、精确argv、同PID/start/boot和单一连续session已核对，保留两次过严断言拒绝及原回执，窗口未重启。见[交接健康](evidence/observation-health-at-handoff.json)。不要在窗口内改工具、再开writer、发搜索、COUNT、重启替换App、改DB或权限，不阻止自然睡眠，不静默延长或补窗。
- 旧原件恢复图：`.build/topk-consistency-20261019/install-state/adopt-plan.local.json`；原始真实DB与旧App路径以此为准，不按模糊名字清理。需要恢复时先核对精确进程/身份并正常Quit，使用同目录install_topk.py的adopt --restore；当前未请求恢复。
- 已按期封存：1426点/1421有效CPU区间，各85177.990530秒；4段睡眠缺口已排除。监听9分钟高CPU与11:08 revision+7,341,204、GUI多次台阶及89.485MiB峰值、普通安装footprint+17.690%未解、serve RSS下降但footprint保持均保留。见[结束摘要](evidence/observation-finish.json)与REPORT收尾节。上面的RUNNING段为原启动时状态，以本节为当前状态。
- 原始33文件和53份阶段核查已逐字复制至ignored `.build/topk-consistency-20261019/sealed-24h-20261020`，原件/冻结/真实回滚/旧67点均保留。实际worker已退出，勿再次启动采样器。
- 同一heartbeat继续已授权后续，不在本窗口结束时暂停：先按精确清单归档并清理过期临时库，再依[下一轮STATUS](../watch-recovery-20261020/STATUS.md)推进日志/恢复范围/写放大及GUI定位，完成全部验收或明确需人工处理后才暂停。不自动重开观察，不拼接数据。
