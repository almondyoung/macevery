# MacEvery修复版24小时观察记录

2026-10-21，r4，OBSERVING_WITH_WATCH_CPU_RECURRENCE，最终结论待到期。固定App为0.1.3-g29b6140，源码 `29b6140575cdd69c965732b943dccda77e8b586e`，当前运行进程继续使用；本轮没有重启或替换。

用户授权从2026-10-21 10:18:28.343991至2026-10-22 10:18:28.343991（Asia/Shanghai）独立观察24小时。60秒采样已启动，实际单一worker、启动cookie、arm64 UUID、boot和精确argv核验通过；初始App/DB身份匹配，三个角色已识别。首端点在起点后36.826秒，之前无CPU差分覆盖，不补采或延长窗口。[启动摘要](evidence/start.json)和[持续状态](STATUS.md)记录复核入口。

| 首端点角色 | RSS | physical footprint |
|---|---:|---:|
| GUI | 112.297MiB | 50.485MiB |
| 搜索服务serve | 73.781MiB | 1543.924MiB |
| 监听watch | 8.703MiB | 3.922MiB |

这些值是本轮普通使用起点，不是性能A/B。serve的RSS较低而footprint仍约1.51GiB，不能说索引内存已释放。当前App及精确源码、上轮受控验收/普通资源点与异常收尾记录已冻结，旧版App及真实索引回滚保持；旧观察数据不加入本窗口。

重点检查有效同身份区间的监听单核CPU是否再次≥50%、连续时长、CPU秒和revision变化，并保存能取得的同期触发日志；GUI与serve按每小时footprint中位/p95/峰值、斜率和尾部平台判断是否持续增长。RSS单列，睡眠、缺口、身份变化与未知区间明确排除，不能把单个台阶或重启前后差值称为泄漏。

源诊断日志有容量/频率和事件采样上限，每小时保存一组有界原件快照，可能遗漏两次快照间已轮转的内容。资源采样也可能漏掉分钟之间的CPU/内存短峰。旧监听具体触发path/flags未记录、旧GUI台阶原因、历史普通点footprint+17.690%及上轮serve普通点+2.717%仍是独立未解事实，不用本轮起点覆盖。

正常无变化不通知，显著异常或需要用户操作时说明；到期后核实采样器自然退出，保留原始数据/哈希和完整异常时间线，汇总实际CPU覆盖、缺口、各进程身份内趋势及未覆盖项。当前没有长期稳定或无泄漏PASS结论。

## 14:38至15:13监听异常

本窗首次显著监听高CPU复发已通知。[脱敏异常证据](evidence/watch-recurrence-1438.json)冻结至15:18:28、seq301的已提交前缀，App/DB均301点match、三角色各一个身份、单一collector_session，仅initial_endpoint barrier；实际worker/start/UUID/boot/argv及writer锁另经health核验，未重启或变更观察窗口。

18个有效约一分钟区间watch单核CPU≥50%，峰81.625992%；这18段合计CPU825.388416秒、有效清醒1079.994083秒。按连续序号分为14:39–14:40的2段与14:57–15:12的16段，不把中间低于50%的区间删掉或称作没有扫描。整个扫描过程由下表的原始start/complete日志另证。

| 全根恢复，Asia/Shanghai | indexed=rows_written | 日志elapsed | stale_removed |
|---|---:|---:|---:|
| 14:38:47.785→15:00:37.018 | 7,364,441 | 1309.356秒 | 1 |
| 15:00:37.375→15:06:54.471 | 7,369,360 | 377.099秒 | 12 |
| 15:06:56.587→15:13:00.000 | 7,369,361 | 363.416秒 | 0 |

三轮合计22,103,162次访问/写入工作、日志elapsed2049.871秒（约34.16分钟），不是不同文件数。三次资源端点观测到revision分别增加7,368,580、7,407,882、7,369,362，last_indexed_at均前进；不能把revision增量与上表计数当作同一种指标或不同实际文件修改数量。indexed_at公开代次语义仍保留，本轮prepared upsert不会跳过新代次写入。

三组恢复记录的原始recovery flags均为3。精确受测源码定义MustScanSubDirs=1、UserDropped=2，callback将原flags送入FsEvent，action_for_flags因UserDropped选择全根恢复。三轮均scope_fallback=false、retry_due=false、warnings=0，故已有证据支持带UserDropped恢复标记的三轮roots扫描；不能据此确定丢事件由何种工作负载、回调/系统压力、排队或自写引起，也未证明每轮全扫必要或多余。前两批event_count=4096是处理批上限，不是完整队列深度或丢事件原因的测量。

事件内watch采样峰RSS109.328MiB、footprint98.001MiB；15:18末footprint14.657MiB，高于此前约5.110MiB平台；最近前端点14:38:28为5.407MiB。最近五个CPU区间为约0.01835%、0.01972%、0.02870%、0.01686%、0.01809%，短尾部回落仍需后续观察。GUI同期footprint峰66.579、末52.064MiB；serve从1543.924降到1501.642MiB。没有进程身份变化，不能用重启解释差值，也不能把这些台阶/回落归因为某个唯一机制或称无泄漏。

独立只读复核确认资源高CPU计数和峰值、日志三组start/complete、flags映射及扫描总量。首次异常发现后额外保存311,567字节的active/rotation原件、SHA及FD/路径/尾行完整性判据；两文件均可解析，扫描完整记录无warnings或I/O failure。原始私有记录保留精确事件ID/带随机状态路径摘要，公开证据去除这些主机细节；采样、限频和轮转仍不能覆盖全部原始触发。较早的hour-004日志快照与本次异常副本分别保存，不将前者当作当前日志。

原24小时窗口继续至10月22日10:18:28，不发测试搜索、不改App/DB/工具或权限、不补窗。后续重点跟踪新增高CPU段、事件后watch内存平台与GUI/serve同身份footprint；到期统一报告全窗实际覆盖和完整异常线。

## 16:29至16:36新增监听异常

本窗又发现一次显著复发，[独立脱敏证据](evidence/watch-recurrence-1629.json)保留至17:23:28、seq426的资源前缀及此后读取的有界日志副本。新增六个有效约分钟区间端点为16:30:28至16:35:28（seq373–378），watch单核CPU依次73.290561%、80.045420%、79.488340%、82.080153%、79.286140%、80.772196%。六段合计CPU284.977806秒、清醒359.999676秒；本窗累计24个≥50%区间，新的全窗峰值82.080153%。扫描完成落在下一段，16:36端点该段CPU仍30.787609%，不能将只计≥50%的六段当作整个扫描CPU成本。

| 新增全根恢复，Asia/Shanghai | indexed=rows_written | 日志elapsed | stale_removed |
|---|---:|---:|---:|
| 16:29:33.497→16:35:49.050 | 7,369,600 | 375.548秒 | 0 |

start/complete属于同一recovery事件、原flags=3（MustScanSubDirs|UserDropped），event_count=2，scope_fallback/retry_due/retry_roots均false，retry_paths/warnings均0。16:36端点revision由33,390,339增至40,759,972（+7,369,633），last_indexed_at从15:12:54推进至16:35:43。资源元数据约每分钟读取，不能推定精确事务时刻或将revision增量等同于不同文件修改数；本轮观察也没有发搜索或做逐文件正确性验收。与前次相同，全根恢复路径已证实，UserDropped的底因和恢复是否可以合并/避免仍未建立。

| 同身份角色 | 本次扫描内峰值footprint | 17:23末footprint | 最近两完整小时中位footprint |
|---|---:|---:|---:|
| GUI | 52.204MiB | 51.626MiB | 52.142→52.142MiB |
| serve | 1501.595MiB | 1501.595MiB | 1501.642→1501.595MiB |
| watch | 97.204MiB | 13.844MiB | 14.719→13.844MiB |

表中最近完整小时为15:18至16:18、16:18至17:18，各60端点；17:18之后为部分小时，不能当作完整小时。watch本次峰RSS107.969MiB，16:29最近前端点footprint已为13.844MiB，16:36回到同一值，此后至17:23末尾15点保持13.844MiB；GUI尾15点范围51.595–51.626MiB，serve为1501.595MiB平台。本次末RSS依次GUI118.328、serve975.094、watch28.766MiB，不能用RSS替代footprint判读。最近几小时没有GUI/serve持续上行证据，watch有再次短峰后回落；仍保留首轮较高平台、历史台阶和未解释的普通安装点，不作无泄漏或整体内存改善结论。

截至该资源前缀，各角色426点known且各一个进程身份，App/source和DB pin均426点match，collector_session仍一个，barrier仅initial_endpoint；各角色有效CPU覆盖25463.195430秒（7小时4分23.195秒）。本窗口仍在运行，首端点前36.826秒无CPU差分覆盖；不能把目前未见睡眠/缺口写成最终24小时无缺口。

本次额外保存active/rotation共432,915字节，565条可解析记录覆盖16:13:09.680至17:23:18.816，FD大小/mtime、路径inode和尾行完整性均通过，SHA逐文件保留。可见范围无未配对roots记录，但三条记录合计报告729次诊断记录尝试被抑制（单条最大451）；这是成功追加前积累的诊断限频等抑制计数，并非FSEvents丢失量。源事件采样、限频、轮转和快照时机仍限制覆盖，不能称完整原始触发审计。独立只读复核核对新增六段、成对扫描、计数/flags和revision前进；第一次异常副本与证据未覆盖或改写。

当前App与原观察窗口继续保持，下一步仅跟踪新增复发和内存趋势，到10月22日10:18:28到期封存后再给全窗汇总。

## 18:48搜索服务footprint台阶的只读初查

按用户追加要求，只读检查已提交样本、固定29b6140源码、采样定义和Apple公开API实现；没有发search/status/reload请求，没有附加进程、采堆/VM快照、改库/源码/工具、构建或重启。此项先保留证据并继续自然观察，到期合并汇总。[脱敏证据](evidence/serve-footprint-1848.json)冻结至20:01:28、seq584，实际worker身份/锁和committed prefix另由原health helper核对。App/DB均584点match、三个角色各一个身份、单一session，仅initial_endpoint barrier；监听≥50%仍是已通知的24段，无新增。

| 已提交端点，Asia/Shanghai | serve footprint | serve RSS | 证据含义 |
|---|---:|---:|---|
| 18:47:28，seq510 | 1501.517MiB | 974.875MiB | 跳升前端点 |
| 18:48:28，seq511 | 1549.095MiB | 905.109MiB | footprint一次+47.578125MiB，RSS−69.765625MiB |
| 18:49至19:40 | 1549.111MiB | 单列原始样本保留 | footprint平台，未恢复到跳升前 |
| 19:41至20:01 | 1549.127MiB | 20:01为7.813MiB | RSS于19:47/19:49再次明显下降，footprint保持 |

18:48区间59.992秒内，serve累计CPU只增0.000700791秒（单核0.001168%），revision仅+4至40,763,355，last_indexed_at仍为16:35:43。同区间GUI footprint+32KiB、watch−16KiB，均无同量级台阶。其后73分钟serve footprint仅18:49和19:41各再增16KiB，范围1549.095490–1549.126740MiB，累计CPU增加0.037764417秒。19:47/19:49的RSS另降至约629.109/7.859MiB，末为7.813MiB；不能用RSS下降消解footprint增长或声称索引内存已释放。

这不支持把本次直接归因为一次通常的全索引搜索/重建负载，但不能仅凭一分钟revision+4断言serve只处理4条：`loaded_stamp`可能早已落后，显式reload也不以last_indexed_at为门禁。历史窗口没有持续记录serve请求、reload计数或前后arena容量。原生GUI在[服务管道处理](../../macos/MacEveryApp.swift#L1297)排空并丢弃stdout/stderr，watch的有界日志也不是serve分配审计；缺少重载日志不能写作没有重载。

固定源码的内存所有权复核确认：

- [ServiceState/backend](../../src/server.rs#L172)持有一个常驻索引；[full_reload](../../src/server.rs#L321)先invalidate/drop旧backend再加载，写锁防止并行搜索持有旧backend，未见长期双份索引容器。Rust对象释放不等于底层分配器已把页交还系统。
- [增量更新](../../src/server.rs#L420)在应用后显式drop变更Vec；[读取限制](../../src/db.rs#L836)为4096事件/ID、32MiB字符串payload，并非整个暂存/进程内存上限。没有后台刷新循环，status不触发load；搜索ensure_fresh或显式reload才改变索引。
- [查询](../../src/compact_index.rs#L1121)的PreparedQuery、TopK和结果均局部，未见跨请求结果缓存。[HTTP序列化](../../src/server.rs#L642)暂时同时持有结果、JSON body和完整response，调用返回/报错后释放。caller-supplied limit无硬上限、客户端断开不终止已开始同步扫描，是有相关大请求时的峰值/取消风险，本窗没有证据将其关联本次台阶。
- [字符串arena](../../src/compact_index.rs#L1038)会按需倍增，单块通常至64MiB；替换/删除的旧字符串不逐条回收，死字符串超过32MiB等预算才要求重载。这是保留容量候选；小批变化也可能跨容量边界，但没有本次容量明细，不能认定发生或将预算数值拼成47.578MiB根因。

采样定义也经独立静态复核：[RusageInfoV0](../../tools/check_service_round2.py#L127)与当前SDK字段顺序匹配；CPU原值为Mach timebase units，脚本转换一次为ns的方向正确，RSS/footprint直接按字节保存。Apple公开[XNU说明](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/task.c#L1144-L1169)将phys_footprint记为包含internal_compressed等多项ledger的余额，RSS使用不同口径；[时间单位定义](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/recount.h#L123-L124)与[换算说明](https://developer.apple.com/library/archive/qa/qa1398/_index.html)用于排除明显的单位误读。公开主分支没有与本机运行内核版本逐项绑定，不能由这些定义证明本次压缩、换页或某个ledger变化。

**当前结论：根因UNKNOWN，尚未证明不必要保留或泄漏。** 可继续核查的候选包括arena容量/死字符串、分配器保留已释放页、已有分配的驻留/系统记账变化。当前历史数据没有compressed/private dirty/allocated分项或分配栈，无法把47.578MiB分摊到这些类别。也未建立持续数小时爬升：目前是一次较大台阶后平台，GUI约53.2MiB小平台另保留。

九个相关源码/工具文件已与固定提交逐字匹配并保存SHA；资源前缀、端点摘录和三项独立复核在 `.build/watch-observation-20261021/investigations/serve-footprint-1848-20261021`。原始数据/工具和App均保持。本窗不通过新请求、堆附加、强制内存回收或临时诊断构建验证候选；后续继续按同身份跟踪新平台是否再升，到期将此台阶、RSS变化及全部监听异常一并总结。
