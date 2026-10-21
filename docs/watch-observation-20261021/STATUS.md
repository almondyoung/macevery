# MacEvery修复版独立24小时观察

2026-10-21，r3，Asia/Shanghai，OBSERVING_WITH_WATCH_CPU_RECURRENCE。Owner：当前聊天。

## 固定范围

- 用户授权：固定当前版本，独立再观察24小时；重点看监听高CPU是否复发、GUI和搜索服务footprint是否持续增长。正常无变化不通知，到期汇总；基线、异常和回滚保留。
- 固定App：0.1.3-g29b6140，源码 `29b6140575cdd69c965732b943dccda77e8b586e`。当前App原位继续运行，未构建、重启或替换；deep/strict签名、实际三角色父子关系和Mach-O UUID已核对。
- 窗口：2026-10-21 10:18:28.343991至2026-10-22 10:18:28.343991（Asia/Shanghai），86400秒，60秒间隔。首端点10:19:05.169777，起点后36.825831208秒；此前无相邻CPU区间，不补齐、不挪起止。
- 本机ops/observation/report。允许只读资源、App/DB控制身份和有界触发日志快照；禁止搜索、全表COUNT、改真实files/配置/权限、源码或工具变更、构建/负载、重启替换App、自动续开或拼接窗口。

## 状态与入口

- [启动摘要](evidence/start.json)、[观察报告](REPORT.md)。本轮独立私有目录 `.build/watch-observation-20261021`；所有旧窗口、异常、冻结版及真实索引回滚原件保持，不合并数据。
- 单一有界采样器（仓库根目录下的等价命令示意，勿重复执行）：`tools/observe_app_resources.py run --state .build/watch-observation-20261021/observation-24h`。实际绝对argv以私有launch回执为准，未改进程启动参数。已核对实际PID/start cookie/arm64 UUID/boot/精确argv和单一collector_session，不能只信state.running。writer锁已被同一worker持有；未穿插sample。
- 首次启动回执校验误取通用Python的首条x86_64 UUID而失败；已从全部Mach-O项匹配实际arm64 UUID，以同PID/cookie/boot/argv及全部已提交样本的单一session证明没有重启或重开。原launch脚本、request及修正说明均保留。
- 新冻结基线在 `baseline/`：当前App副本、精确源码archive、上轮REPORT/STATUS/最终验收及前后资源点、旧24小时收尾/封存清单、当前两份有界触发日志；文件SHA逐项记录。真实DB仅保存控制身份/配置，不再复制大测试库；既有完整真实索引回滚保留。
- 当前配置模式memory/Fastest，roots/excludes及快捷键保持；身份/配置在 `preflight.local.json`，工具冻结哈希在 `monitoring-tools.local.json`。
- 每次跟进先运行私有 `health_check.py`，核对committed prefix实际长度≥state.journal_bytes、连续sequence/行数/末记录与state相同，验证观察工具哈希、实际worker/cookie/UUID/argv/boot、锁、新鲜度、source和DB pin；再运行官方工具summarize。已有helper SHA须先与冻结清单核对，不修改helper绕过异常。
- `health_check.py`只在本目录保存缓存/快照。每小时最多一组active及一个轮转文件，共25组、原始字节上限12.5MiB；不获取诊断writer锁。记录FD前后size/mtime及路径inode是否一致、尾行是否完整。两次核查间可能轮转丢细节，源日志本身也采样/限频，不能称完整原始事件审计。

## 判读和通知

本窗已记录并通知的显著异常见[14:38监听复发证据](evidence/watch-recurrence-1438.json)：14:38:47至15:13:00三轮全根恢复，原始recovery flags均3（代码中的MustScanSubDirs|UserDropped），均无scope_fallback/retry_due/warnings。三次indexed/rows_written合计22,103,162，是重复扫描工作量而非不同文件数；公开indexed_at变化仍导致写入。当前证据不能确定丢事件起因或每轮恢复是否存在可避免重复。

截至seq301/15:18:28，18个有效约分钟区间watch单核CPU≥50%，峰81.625992%，这些区间合计CPU825.388416秒；两组为14:39–14:40的2段、14:57–15:12的16段。三次revision分钟增量7,368,580/7,407,882/7,369,362，last_indexed_at均前进。最近5段CPU约0.0169%–0.0287%，不能只凭短尾部称事件长期恢复。

监听峰RSS109.328MiB/footprint98.001MiB，事件后footprint14.657MiB仍高于此前约5.110MiB平台（最近前端点14:38:28为5.407MiB）；GUI同期峰66.579、末52.064MiB，serve footprint从1543.924降至1501.642MiB。均按同身份记录，未重启App/worker或改库，尚不能据此判持续泄漏或整体内存改善。额外有界原始日志及完整分析在 `.build/watch-observation-20261021/anomalies/20261021-071833-559634`，与较早的hour-004快照分别保留。异常已通知，后续仅新增复发/显著趋势或需操作时再提醒，不重复播报旧18段；原窗口/截止不变。

1. 用已提交JSONL计算本小时和全窗，而非将累计hourly summary当成单小时结果。按每角色、同一进程身份列每小时CPU/RSS/footprint中位、p95、峰值、端点、斜率和尾部平台；记录实际样本数/有效CPU时长。单次台阶不直接定为泄漏，RSS回落不等于footprint回落。
2. 监听检查每个有效无barrier区间的单核CPU，特别记录≥50%的分钟区间、连续性、CPU秒、revision delta/last_indexed_at及同期日志。新日志可辅助判读，不能猜未捕获的path/flags；恢复必要性需独立证据。
3. GUI和serve检查同身份footprint小时中位是否连续上行、幅度、斜率、尾部是否回落或平台。重启/模式切换另分段，不跨身份连线；短时峰值与持续增长分开。显著复发、持续增长候选、缺样/worker退出或身份失配等才通知；正常无变化安静。
4. 睡眠、power UUID变化、未知时钟/进程及采样缺口按现有barrier规则排除CPU连线；记录wall窗口、实际清醒覆盖及缺口。锁屏不需要UI操作；不加唤醒锁、不改系统权限。

## 16:29新增监听复发

[新增异常证据](evidence/watch-recurrence-1629.json)冻结至17:23:28、seq426：新增seq373–378六个有效分钟区间（端点16:30–16:35）watch单核CPU≥50%，峰82.080153%，这六段CPU284.977806秒/有效清醒359.999676秒；本窗累计24段。16:29:33.497至16:35:49.050的成对日志证实又一次roots恢复，原始recovery flags=3、event_count=2，indexed=rows_written=7,369,600，elapsed375.548秒，stale/warnings/retry/fallback均为0或false。16:36端点revision+7,369,633、last_indexed_at前进。日志和控制元数据不能证明所有文件内容正确，丢事件底因与该轮全扫是否可避免仍未知。

watch本次峰RSS107.969MiB/footprint97.204MiB，16:36已回到13.844MiB，尾15点相同；17:23 GUI/serve footprint为51.626/1501.595MiB，最近两完整小时GUI中位均52.142、serve1501.642→1501.595MiB，未见持续上行。此前峰值和较高watch平台继续保留，不据此作无泄漏结论。本次末端三角色仍各一个身份、App/DB均426点match、单一session、仅初始barrier；有效CPU覆盖25463.195430秒（约7小时4分23秒），24小时尚未结束。

额外active/rotation原件共432,915字节及SHA、完整性判据、独立复核在 `.build/watch-observation-20261021/anomalies/20261021-092414-548603`。565条可解析日志中报告合计729次诊断记录被抑制，不能称完整触发审计，也不能将其等同于丢失FSEvents数量。已通知新增6段；后续不重复播报既有24段。原App、采样器和10月22日10:18:28截止均不变。

## 到期

1. 先核实window_finished、last_exit=window_elapsed/0，实际worker已退出、writer锁空闲、原始文件停止变化；不能把signal stop/exit0或state.running当作正常到期证明。
2. 保存manifest/state/journal/hourly/启动回执和本轮诊断快照的逐文件SHA、完整异常时间线及脱敏逐小时摘要；原件与封存均保留，旧67点和旧24小时不拼入。
3. 更新本REPORT/STATUS和本周文档登记。沿用用户授权提交推送收尾记录并精确回读远端SHA；当前App保持29b，不随文档HEAD重建。
4. 汇报监听复发情况、GUI/serve同身份趋势、实际覆盖/睡眠缺口及未覆盖项。窗口到期不是连续24小时稳定或无泄漏PASS。完成后暂停同ID `macevery-48`，保留证据、不归档聊天；不自动开始下一窗口。

## 历史边界

上轮[报告](../watch-recovery-20261020/REPORT.md)和[旧24小时收尾](../topk-consistency-20261019/REPORT.md)保持：监听旧9个≥50%分钟区间、峰85.635%、revision一次+7,341,204；旧GUI footprint峰89.485/末66.032MiB且历史台阶根因未证实。TopK安装普通点footprint+17.690%未解；监听修复轮普通serve footprint+40.844MiB/+2.717%另保留。新窗口不覆盖这些事实，也不拿serve低RSS证明索引已释放。
