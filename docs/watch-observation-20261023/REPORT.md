# MacEvery b027独立24小时观察记录

2026-10-24，r8，原窗口自然到期，已封存；结论为 `SEALED_WITH_USERDROPPED_HIGH_CPU_AND_ROLE_COVERAGE_LOSS_NOT_STABILITY_PASS`。固定 App 0.1.3-gb027a42、source b027a42c2cb859235b2a354c4c4bd8187a785a77 保持不变。本次只完成观察收尾，没有重启、替换、构建、App请求或数据库查询/写入，不自动另开观察。

## 结果与实际覆盖

原窗口为2026-10-23 15:24:53.957679至2026-10-24 15:24:53.957679（Asia/Shanghai），独立86400秒。最终 `window_finished=true`，`last_exit.reason=window_elapsed`、exit_code=0；实查原worker消失、collector清单空、writer锁空闲，原始文件两次独立检查稳定。不是signal stop被误记为到期。末端continuous容差+0.035020458秒原样保留，末单点不当完整第25小时。

1277条JSONL／2,952,152字节，sequence连续、完整末记录与state相同、唯一session；source/DB pin各1277条match。原始SHA为 `669f182a3770a26b3a9684a772e16220bd4ee41ea1cb26014435d4b624364fdf`。四个private helper、两个public tool、当前App文件及冻结source archive实际SHA均匹配启动固定值。App不会随文档HEAD重建。

三角色各539个已知端点、533个严格有效CPU区间，**每角色有效awake 31979.188909秒（8小时52分59.189秒，名义窗口约37.01%）**。GUI/serve/watch有效CPU分别21.861294／7.434545／520.974061秒。最后已知为10月24日02:05:54.517，首次missing为02:06:54.522；退出只约束在两端点之间，原因UNKNOWN。至终点每角色738个missing端点，首次missing到终点约13小时18分。原PID实际消失、未见替代App进程；不把缺失当0 CPU、内存释放或稳定。此后不能据无新增日志判断没有UserDropped或重载。

首端点前0.057852083秒CPU无覆盖。端点continuous跨度86399.977168秒，实际wall跨度86400.473025秒，两种时钟分开报告。10个sleep/gap区间的sleep差合计10071.467897秒（约2小时47分51秒）；5个power UUID屏障不等于5次独立睡眠。屏障另排除awake 685.079030秒，其中角色仍已知时205.396494秒。1441个名义slot含终点，只捕获1277个，缺164个不补。16个有界日志slot共16,310,042字节，缺槽3/4/10–16不补；启动163字节仅configured，非started证明。详细边界见[覆盖清单](evidence/coverage.json)。

## UserDropped、恢复与CPU

两段复发时段内，共确认四批UserDropped根恢复完成：

| 本地开始—完成 | admitted_raw带drop记录 | batch OR／原始recovery flags | indexed | 实际写／跳过未变 |
| --- | ---: | --- | ---: | ---: |
| 10月23日16:10:56.123—16:14:24.368 | 10 | 3／3 | 7,397,721 | 590／7,397,131 |
| 16:14:24.527—16:17:49.243 | 6 | 163843／3 | 7,397,721 | 0／7,397,721 |
| 10月24日01:02:14.382—01:05:02.789 | 20 | 3／3 | 7,393,204 | 87／7,393,117 |
| 01:05:02.955—01:08:10.767 | 2 | 3／3 | 7,393,204 | 0／7,393,204 |

寿命callback标志记录0→10→16→36→38；38不是独立OS drop次数或丢文件数。四批各有不同recovery ID，第二批的新增drop在第一批工作期间已到达；零写不能直接判为不必要的重复恢复。合计29,581,850次indexed访问、677行写入、29,581,173行未变跳过，stale_removed/warnings/legacy_fallback均0。新增实现避免了这些未变行的代次DML，仍有全根遍历CPU；检查点元数据和物理/WAL写字节未完整审计，不能称消除全部写放大。UserDropped操作系统层底因、每个原始触发path及是否有可进一步合并的恢复义务仍UNKNOWN。

全部process-measured >=50%分钟候选11个，严格source/DB匹配且无sleep/gap/clock/identity屏障集合也为11个，未因unknown隐藏高段。分为16:10:54–16:12:54的2个、16:13:54–16:17:54的4个、01:02:54–01:07:54的5个；第一轮中间一分钟41.8985%，不叫连续6分钟。峰72.208859%，高段CPU450.896225秒／有效awake660.016787秒。分钟均值不能排除更短峰值。全部原分钟及revision/last_indexed_at见[CPU区间](evidence/cpu-intervals.json)、[全部高候选](evidence/all-high-cpu-candidates.json)。

最终日志合并2053条可归属watch记录和543条serve记录，包含16:23额外有界诊断，未用小时日志中的缺失覆盖第一轮已确认恢复。留存子集retry_paths/retry_roots/retry_due/warnings/legacy_fallback/inbox_overflow/worker_overflow/worker_scope_failure/oversized/io_failures均无阳性记录；这仅约束留存子集。源限频、suppressed计数、轮转、部分首尾、FD/路径竞态和缺槽元数据全部封存，不声称完整OS触发审计。

## 加载与footprint

可见完成的全量加载为两次`event_limit`：15:25:39.600–15:25:42.341（wall2.741秒／CPU2.719537秒）和17:34:14.864–17:34:17.505（wall2.641秒／CPU2.624612秒）。可见`history_gap`成功全量加载及snapshot_reconcile均0；按同一实际serve boot/PID/cookie/UUID/executable/session/sequence核对加载与增量计数，未发现仅idle缓存计数增长但缺after边界的未闭合加载候选。最后load/success=3/3含启动，incremental_attempts/applies=4/2，queries=8/8；没有主动发查询复验。旧idle方法/phase不重复当新操作，缺日志不能证明未覆盖部分0次。

首event_limit去重仍4751 ID超过4096预算，只有24次额外重复事件不能解释为冗余重载；精确delta target没有持久，full_after另一个读事务不能充作判定快照。正常full load只读重建内存，不产生上游files更新。具体冗余更新与物理写放大未闭合；完整只读诊断保留于下方原始补记与[evidence/event-limit-readonly.json](evidence/event-limit-readonly.json)。

| 角色 | 退出前最后FP MiB | 同身份、无屏障尾60端点FP首→末 | 尾段判断 |
| --- | ---: | --- | --- |
| GUI | 59.297974 | 59.329224→59.297974 | 约59分钟OLS −0.040089 MiB/小时；末80分钟−0.03125 MiB，局部平台略回落 |
| serve | 1578.220512 | 1578.220512→1578.220512 | 约59分钟完全持平；RSS低是页面/压缩状态，不能当索引FP恢复 |
| watch | 7.813049 | 7.938049→7.813049 | 尾60开头仍处恢复；01:09:54后57点约56分钟保持7.813049，较事前6.875549仍+0.9375 |

这些都是**02:05退出前的历史值，当前三角色FP没有覆盖**。GUI睡眠后曾升到78.798MiB，随后回落，末数小时约59.3；本窗没有重现旧4dc末80分钟持续上行的形状，但不能作因果A/B。serve重载/页面状态关联后升到约1578MiB，后几小时仅约0.06MiB微动并在末段持平；全量加载期间瞬时FP回落未维持。watch两轮恢复采样峰约15.001／14.157MiB，恢复后有较高残留平台；第二轮前连续约4.30小时6.844→6.876MiB，OLS约+0.0075MiB/小时，不能把这种小幅变化直接判泄漏，具体分配/allocator归属未知。局部平台不证明无泄漏，整个后半窗缺失也不能推定平台持续。

[每小时CPU CSV](evidence/hourly.csv)、[每小时分段内存CSV](evidence/hourly-memory.csv)、[完整逐小时JSON](evidence/hourly.json)、[各身份/屏障段与尾60](evidence/memory-segments.json)分开保留。24个小时各自统计，空覆盖小时用unknown/null；CPU跨小时按分钟均匀分配仅估计并保持原CPU/awake总量。内存按实际身份、collector mode及source/DB/睡眠/power/gap/capture屏障分段，不跨缺口连线；mode=run只代表collector模式，serve backend通过历史exact argv验证，不代表完整逐分钟App模式审计。p95为最近秩，斜率为已观测端点OLS及Theil–Sen；非正内存单列无效，本窗无此有效端点异常。

## 封存、未决与历史保留

本轮私有根的`final-archive-20261024`保存230个原件/冻结基线副本，共34,875,751字节，包括manifest/state/journal/hourly/启动回执、四private/two public工具、全部小时有界日志、四个额外诊断目录、冻结App/精确源码/验收资料；后续派生摘要、完整已识别异常时间线和逐文件SHA单独登记。原件继续原位保留，pycache不作证据；较早窗口/67点/8096e0a/a4418a7/29b/4dc冻结App/source及所有真实索引回滚未清理、未拼接或重建。原始私有文件不推送，只提交脱敏摘要与SHA锚点。

[最终机器摘要](evidence/final.json)、[全部已识别异常时间线](evidence/anomaly-timeline.json)、[逐文件封存索引](evidence/seal.json)提供追溯。提交/精确远端SHA及跟进停止回执在证据封存后独立保存，避免自引用改变原件。已同步本周个人项目文档登记，不计PFT人天；登记仍为本地稿/待共享，未扩大公司访问权限。

仍需另行授权的新工作：OS UserDropped底因、根恢复义务进一步合并的正确性、event_limit下更细的有界刷新策略、watch恢复后FP页面归属、App三角色退出原因。当前窗口已结束，不自动重启/修复/扩窗；不能称连续24小时稳定或无泄漏PASS。

历史未决保留：4dc窗口1轮UserDropped恢复/6个>=50%分钟峰76.03%，完整扫描7,390,140行代次写放大与19:25history_gap；GUI末80分钟+3.766MiB；旧29b4轮drop/24分钟>=50%峰82.08%与serve+47.578MiB；更早9分钟峰85.6349%/revision+7341204未持久原始触发；TopK普通安装总FP+17.690%、29b serve+40.844MiB/+2.717%、4dc普通serve+0.969MiB及短词尾FP+2.320MiB；b027普通serve+5.453MiB/+0.349%和history_gap复用原索引FP高于基线重载后均不是唯一归因或整体降低。新窗口不是历史优化因果AB。

## 观察期补记（r7原记录，时间与暂态判断保留）

以下为当时的只读定位和阶段结论，涉及“观察中/待封存”的语句仅表示各次记录时点；当前状态以上方r8最终结果为准。

## 启动复核

前三个committed端点/119.955秒CPU区间、实际worker/锁/单session/App/DB/三角色核验通过，无初始外sleep/gap/身份屏障。首端点RSS/FP MiB：GUI149.3125/65.7823、serve1571.1719/1567.3767、watch17.7031/5.2193；普通使用起点，非受控AB。短序列不判持续增长或平台。

首slot有界日志保留44 watch/4 serve窗内且prefix截止前可归属记录：UserDropped原始/OR/寿命累计尚无可见增长，高CPU>=50%分钟0；缺日志不证明全窗0。serve捕获1次event_limit full_after/fallback_full，history_gap full_after及snapshot_reconcile各0可见记录；两者不能混称。FP从首端点约1567.377回落至1350.861MiB，RSS反而上行，容量/allocator/VM唯一归因未闭合。后续同身份趋势继续由既有JSONL记录，不另插样本。

raw与first检查证据只保存在本轮私有根；启动SHA及约束见evidence/start.json。正常无变化安静，到期统一汇总。

## event_limit只读诊断，15:50暂记

15:25:39.600至15:25:42.341新增1次成功fallback_full/event_limit，wall2.741秒、CPU2.719537秒。event/id增量上限各4096；此次不进入仅适用于history_gap的snapshot核对。base到full_after的journal完整4775条（4578更新/197新增），4751不同ID，只有24次额外重复出现；首端点时就4771事件/4749 ID，已超两道预算。delta判定和full load是两个读事务，精确判定target未持久，不能把4775当触发时精确数量。按现有预算回退有依据，不能据重复事件认定重载冗余；分批/扩大有界核对reason属于未实施的后续方案。

正常加载只读重建内存，不是这些files写入的来源。journal没有旧字段值，更新触发器包含indexed_at，不能判断4578次更新是否纯代次写放大；也无物理/WAL写字节。可见watch日志子集61次subtree扫描、写44/跳过未变9，无根恢复/legacy回退/UserDropped标志或累计增长；子集不覆盖全部revision范围，不能据缺日志断言全窗0。

索引records+197、capacity仅+1107字节。完整加载瞬时FP1567.439→902.877MiB，随后分钟端点1350.861；15:34端点又到1575.877（+225.016）。同身份VM日志中reusable238.078→0、internal+224.25MiB，RSS反降13.828MiB，加载/查询/容量计数不变；idle的VM是新读、索引摘要是缓存，不能把cached fallback重复算加载。关联已闭合至页面状态变化，具体所有权/allocator归因仍未知，不能判无用保留、泄漏或内存已释放。

截至15:50:54的27端点/26有效CPU区间1559.963秒，无初始外sleep/gap/身份屏障。serve FP1575.892（较首端点+8.516）、GUI55.220、watch5.204MiB；watch分钟峰1.1876%、>=50%候选0。serve回升后约16分钟微变，不能代替逐小时平台或24小时结论。原worker/精确argv/唯一collector/锁/新鲜度/source与DB复核通过，App/窗口保持；先前沙箱ps/sysctl只读拒绝及同冻结helper沙箱外成功回执分别保留。

[脱敏诊断摘要与SHA](evidence/event-limit-readonly.json)；完整有界诊断16文件、逐条journal/关联和额外尾快照保留于本轮私有根diagnostics-event-limit-20261023。额外快照不补旧小时槽、不代表完整触发审计，不改原始JSONL；到期统一封存完整数据和异常时间线。当前记录为本地补记，待原窗口到期统一提交推送。

## 新UserDropped及监听高CPU，16:23暂记

同一watch原始flags=3的根恢复两批完成：16:10:56.123至16:14:24.368、16:14:24.527至16:17:49.243。admitted_raw分别10/6个带UserDropped标志记录，寿命callback计数从0到16；这不是16次独立OSdrop或丢文件数。第一批处理期间新drop记录进入，第二批recovery ID不同；不能因第二批无写入就默认恢复义务冗余。两批各扫描7,397,721行，写590/0、跳过未变7,397,131/7,397,721、删除0，无warnings或legacy回退。新增实现跳过了这些未变行的代次写入，但没有消除全根遍历成本；本窗普通使用不是与历史严格A/B，底因仍未闭合。

committed sequence58、16:21:54截止：57有效CPU区间3419.974秒；watch有6个>=50%的同身份有效分钟，峰72.208859%，两段合计有效360.011370秒/CPU241.804971秒。第一段16:10:54至16:12:54、第二段16:13:54至16:17:54，中间一分钟41.8985%，不能叫连续6分钟。全部候选也满足严格source/DB/identity/sleep/gap/clock条件，不能把扫描事务暂不推进revision误当无CPU。对应revision先+1342后+590，last_indexed_at在两批完成时更新。

watch FP从根扫描前最近端点5.391MiB到采样峰15.001，再到6.782；恢复结束后约4分钟尾段未再涨，但仍高于事前，不能确认长期平台或判泄漏。GUI约55.360MiB、serve1575.924MiB；服务保留日志加载计数仍2/2、查询4/4，没有新增重载计数。serve RSS降至1083.531MiB时compressed约496.109MiB，FP仍1575.924，不能称索引内存释放。原App、进程身份和观察窗口未改，原worker/唯一collector/锁/新鲜度/实际三角色argv核验通过。

[脱敏异常与覆盖边界](evidence/userdropped-1610-readonly.json)。本轮额外只读日志快照499082字节（最大655360），FD/路径/mtime/首尾完整与截断保留；加committed资源摘录及身份/派生回执共8文件SHA在私有diagnostics-highcpu-20261023-1623。它不补小时槽，analyzer的小时日志摘要尚不自动包含该额外目录，到期时间线必须合并此独立证据并按本窗/committed截止筛选，不能默记无复发。正常观察继续，到期统一封存/提交，冻结版和回滚保持。

## 已解释event_limit后续与内存回落，18:23暂记

17:34:14.864–17:34:17.505再完成一次event_limit全量加载（本窗累计2次、含启动successful_loads=3），wall2.641秒/CPU2.624612秒，full_after revision55650204→55665535、records7397317→7392415、capacity+565695字节。仍是4096预算的保守路径，并非history_gap；delta判定target未持久，full_after范围不能当精确触发快照，也未新增DB查询去反推。17:54:31.190成功增量153个ID、incremental_applies=2；最后请求计数8/8，原采样器和原三角色未变，没有主动发搜索或请求。

GUI在17:34分钟升至70.001MiB后回落到51.923，17:54分钟短峰73.485后回落到约57.954，18:22末58.001。serve完整加载瞬时1576.049→1329.111MiB，下一分钟已1577.986，后续约1578.189；索引容量/VM/页面触达与请求的完整分配因果尚未闭合，不能把短时FP下降当释放，或把GUI峰值视作常驻增长。watch末6.797MiB。单小时阶跃尚不足连续数小时增长候选，继续跟踪而不重复通知。

截止18:22:54的179端点/178有效区间10679.975秒，仅初始屏障；高CPU仍为已通知6个、可见寿命UserDropped仍16，history_gap full_after仍无可见新增。完整逐小时CPU/RSS/FP中位/p95/峰/端点、斜率和尾60保存在hourly-checks/hour-002/review.local.json；[脱敏后续及原prefix SHA](evidence/event-limit-1734-followup.json)。小时快照已可能失去16:10原始drop行，因此最终必须同时纳入已保存额外异常目录，不能把小时子集0行当无复发。本地补记，到期统一封存/提交，App和窗口保持。

## 睡眠缺口和GUI单台阶，20:49暂记

18:55:54之后出现2次power UUID屏障和4个sleep/gap区间，分别18:56:54–19:16:46、19:16:46–19:51:13、19:51:13–20:02:17、20:02:17–20:43:55；另18:55:54–18:56:54仅power_transition。sleep差合计6275.395秒，约205.396秒awake因屏障不计CPU；UUID变化不当独立睡眠次数。截止20:48:54，222端点、216有效CPU区间12959.185秒，实际端点wall跨度约19439.977秒、名义325slot仅捕获222，缺失不补窗。小时日志槽3/4缺失原样保留；睡眠相关、已解释且无需操作，继续安静观察。

GUI在屏障两侧从约57.939至60.032、64.845MiB，不跨缺口画连续增长线。当前20:43:55开始连续段首64.439，下一分钟78.751，20:48末78.767；台阶后仅约4分钟，具体分配和睡眠因果未知，不叫多小时持续增长或泄漏。serve末1578.142、watch6.829MiB，UserDropped寿命累计仍16、>=50%分钟仍6、可见event_limit仍2，history_gap full_after无新增。

[屏障、分段内存与覆盖摘要](evidence/sleep-gui-followup.json)及私有hourly-checks/hour-005/review.local.json保留精确起止和SHA。冻结analyzer的tail60可跨睡眠/缺口，相关跨屏障斜率不用于连续清醒趋势结论；本次另按每个实际身份、模式、屏障拆分段和段内小时RSS/FP中位/p95/峰/端点/斜率，CPU仍用原严格有效区间。只增加派生解释，不改冻结工具、原JSONL、App/DB或窗口；原worker和三角色实际身份/argv、writer锁、唯一session及source/DB pin复核通过。到期同时封存上述缺口和遗漏边界，不能称连续24小时稳定。

## UserDropped再次触发两批根恢复，10月24日01:22暂记

01:02:14.382–01:05:02.789及01:05:02.955–01:08:10.767，同一watch完成两批flags3根恢复，逐原始admitted_raw含UserDropped20/2条、寿命callback16→38。这不是22次独立OSdrop或丢文件数；额外两条在第一批期间到达、第二批recovery ID不同，不因第二批零写就认定恢复义务重复。两批各indexed7,393,204行，实际写87/0、跳过未变7,393,117/7,393,204，stale_removed与warnings均0、legacy_fallback=false；代次写放大已避免这些未变行的DML，全根遍历CPU成本仍在，物理/WAL写字节及OS丢事件原因未闭合。

截至01:20:54的committed prefix494，新增连续5个同身份、source/DB匹配且无sleep/gap/clock/identity屏障的>=50%分钟（01:02:54–01:07:54），峰71.053803%，高段CPU209.091254秒/有效300.005417秒；全部process-measured候选与严格集合相同，没有因unknown排除高段。本窗已确认累计11个高分钟、3段；此前16:10两批恢复及额外异常目录继续保留。revision本次+87，last_indexed_at两批完成时分别推进，CPU扫描期间revision不变不能解释为未工作。

watch FP从最近事前6.876到采样峰14.157，再回落7.813MiB，仍高出0.938MiB；恢复后至截止约12.7分钟，尾部CPU约0.0026%，不据单台阶判持续增长或泄漏。GUI59.329MiB与serve1578.221MiB同期基本持平；服务已保存日志load_attempts/successful_loads仍3/3、incremental_applies2、searches8/8，event_limit完成重载仍2、history_gap full_after仍0可见。RSS低不代表索引FP释放，页面归属及UserDropped底因仍UNKNOWN。

[脱敏异常、覆盖与SHA](evidence/userdropped-0102-readonly.json)。小时槽9原有active+rotation快照已包含完整四条根扫描边界，另保存其8条drop关联行、committed资源473–494、身份/计数与逐小时派生和8文件SHA于私有diagnostics-userdropped-20261024-0121；没有新增源日志快照、资源端点或App请求。原worker/锁/唯一session与三角色实际boot/cookie/UUID/exe/exactargv、App文件SHA和DB pin通过。睡眠及小时日志3/4缺口仍保持，不补足或延长；App/数据库/窗口未改。本地异常补记待到期统一封存提交，持续观察。

## 三角色停止可见与覆盖丢失，10月24日08:29暂记

原三角色最后已知端点为02:05:54.517（sequence539），02:06:54.522（540）起至08:27:54.511（900）连续361个端点均为missing，观察缺失跨度6小时21分。退出时间只约束在两端点之间，不能精确到02:06:54。实查GUI64285/serve74065/watch64297均已不存在，全机MacEveryApp/macevery可执行进程清单为空；原worker实际cookie/UUID/boot/exe/精确argv、单collector/writer锁和新鲜度通过，App仍b027原始SHA、source与DB pin匹配，原窗口未到期。

首次missing与power UUID变化同区间，但sleep差近零、boot未变；此关联不足以解释主动退出、注销或崩溃。顶层匹配App名称诊断文件元数据没有给出本次退出证据，也不能反推没有崩溃；退出原因UNKNOWN。未自动重启App、改变配置/权限或发请求。03:04:54–03:10:54、03:10:54–03:28:13另有两个sleep/gap区间，睡眠差分别300.019及979.157秒，独立记录，不把角色缺失全部归因睡眠。缺失的小时日志槽3/4/10–16不补齐。

最后历史FP GUI59.298、serve1578.221、watch7.813MiB，退出前CPU低；GUI/serve此前基本持平，watch本次根恢复后保持7.813至退出前约58分钟，仍高于事前且不是长期无泄漏结论。当前三角色没有可用FP，不能把missing当零内存或索引释放。每角色只有533个严格有效CPU区间、31979.189秒，之后未知区间不计低CPU；analyzer的最后身份tail60不是当前状态。已确认高CPU仍11个分钟/3段，留存drop寿命累计38、event_limit完成重载2/history_gap0可见记录；进程缺失期间不能以无新日志宣称没有事件。

[角色缺失、有效覆盖与SHA](evidence/role-missing-0206-readonly.json)。最小边界资源、原小时17有界日志SHA/末计数、实际原worker身份和三个PID缺失回执、诊断元数据范围及8文件SHA保存于私有diagnostics-role-missing-20261024-0827。该诊断仅使用已有资源/小时日志及进程身份，没有另取源日志快照、增加资源端点、查询数据库或请求App。继续原worker及既定窗口至10月24日15:24:53.957679，到期统一封存完整原件、缺失/异常和有效覆盖；不重开、补足或延长，不拼旧窗。若用户自然重开App，按新进程身份另段记录，不跨退出连线。当前文档为本地r7补记，待原到期统一提交推送，冻结版和所有回滚保留。
