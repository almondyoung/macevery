# MacEvery Top300一致性修复

更新：2026-10-19，Asia/Shanghai。用户明确授权修复、验证后替换App、提交推送，并重新开始24小时观察。

## 任务与边界

- 本机代码/验证/部署三条工作线；主仓库 `macevery`，基线主干46a6078，运行及冻结App源码8096e0a，版本0.1.3-g8096e0a。
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
- 最终Rust验证：Debug/Release各149单测、各3项bootstrap CLI与3项optimize CLI、420事务差分及prefix集成全部通过；严格all-target Clippy、格式和release build通过。候选CLI SHA256为 `836a9e678254776be807d0cd2c7c46e00021db25d9ab6900572e8ff46b5cbe99`，这是制品哈希，源码提交尚待根节点冻结。见[最终检查](evidence/staging-checks.json)。
- 正式原布局：**REJECTED_COLD_IO**。7,226,486条原快照，候选memory/sqlite各24phase全部正确且小于15秒；CLI rare glob在独立新副本15.014秒超时。SQLite同glob14.532/nohit14.857秒仅接近边界。完整失败保留，禁止用热缓存或延长查询预算过关。
- 已定位：同列新dense索引冷读12.719→0.591秒；仅REINDEX仍11.650秒，37.08秒/Python114MB峰值，未采纳。VACUUM隔离副本55.746秒/Python201MB峰值，主文件5,410,697,216→4,896,702,464字节，ID/revision同；fresh clone原名索引扫描0.590秒。这是一次性物理整理收益，非仅搜索代码或冷缓存清空收益。
- 维护API：显式 `optimize --output NEW_DB --json` 已完成并验收。只读源经VACUUM INTO生成新文件，quick_check及有界双cursor核对完整FileRecord（含ID、顺序和数量），保留control pin；拒绝既有输出、sidecar和活动事务。Driver只在显式 `--prepare-compact` 时运行，成功才把同一个新母库供两版本6views使用；不自动startup或替换真实库。
- 最终隔离矩阵：30k及7,226,486条快照均6views×24phase完成；候选CLI、Low Memory、Fastest的72项完整六字段和顺序全部等于旧Fastest全量oracle，候选全部在15秒内，增改名删计数及清理门禁通过。旧CLI/sqlite仍有各自历史TopK差异，未作为候选豁免。见[30k摘要](evidence/equivalence-compact-30k.json)、[真实快照摘要](evidence/equivalence-compact-real.json)。
- 整理保真与成本：真实库逐行验证7,226,486条；源副本SHA前后相同，ID/revision、roots/excludes/meta保持一致。压缩前后旧memory全部24项有序摘要一致。一次性CLI维护28.087秒、CPU13.852秒、进程HWM101.02MiB；含driver核验共34.229秒，文件5,410,697,216→4,896,702,464字节。该成本独立于查询计时，收益包含物理布局变化，不能称仅代码提升或冷缓存清空。原始、失败和紧凑副本均保留。见[原布局对照](evidence/original-vs-compact-memory.json)与[报告初稿](REPORT.md)。
- 旧观察：**PRESERVED**。已核对启动cookie/UUID/boot停止collector，67个原样本及旧state保留，原heartbeat PAUSED；窗口未完成。
- App：仍为原0.1.3-g8096e0a，未安装候选。用户本轮已解锁，原生窗口确认No Query/Fastest/原计数。新24小时 **NOT_STARTED**。

## 下一步

根节点正在接入独立性能/FSEvents结果并冻结源码；本稿尚不提前给出性能门禁结论。最终Swift真实15秒超时/取消、原生安装与资源回读仍PENDING。全部通过才保留原App/真实索引，停精确owned进程，将只读冻结源生成的等价紧凑索引及已测App替换；恢复Fastest偏好、原生更新验收/最终资源采样，再提交推送记录并开新24h。
