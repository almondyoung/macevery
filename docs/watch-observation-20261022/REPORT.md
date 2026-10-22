# MacEvery 修复版独立24小时观察

2026-10-22，r1，进行中。当前仅完成启动核验，不提供长期趋势或稳定结论。

固定 **0.1.3-g4dc3e4b**；源码 `4dc3e4b5d05136ecb75ca72eaa6dc3f6ce18b114`。窗口从 **2026-10-22 13:22:00.017045** 到 **2026-10-23 13:22:00.017045**（北京时间），每60秒已有唯一有界采样器。App与三个原进程保持，未发搜索/状态请求、改库、重启、重建、换版本或改权限。精确启动证据见[evidence/start.json](evidence/start.json)。

本轮基线含当前App、精确source archive、上轮正确性/性能/原生验收和普通资源点，另保留历史失败及异常。原真实索引回滚保留原位，已有SHA/文件身份留存；未为本轮再复制大库。旧冻结版、旧67点/两个旧24h原始观察、逐小时私有材料与封存记录均保留；本轮不与旧数据拼接。

首端点 `2026-10-22T05:22:00.069570+00:00`，距开始0.052540958秒；此前CPU无覆盖。以下为普通使用起点，非受控A/B。Fastest/memory模式；后续自然模式切换按真实身份另段。

| 角色 | RSS MiB | footprint MiB |
| --- | ---: | ---: |
| GUI | 131.094 | 51.079 |
| serve | 798.094 | 1550.158 |
| watch | 18.172 | 4.954 |

启动已验证：唯一writer、实际PID/start cookie/UUID/boot/executable/exact argv、运行arm64 Mach-O slice、单一collector_session；App fingerprint/DB pin匹配、三角色known，committed字节前缀、连续sequence、行数与state.last_sample一致。private helper经独立静态审查；派生分析有9项合成测试。analyzer仅按固定解释器、绝对路径、无参数运行，默认只更新本轮latest-trends派生缓存；任意--output存在可覆盖其他私有文件的保护缺口，未获本轮使用批准，原review保留。

每小时跟进读取同一worker已提交端点，不增加资源采样点；每个elapsed-hour slot至多一次复制watch active+一个轮转（各256KiB）、serve active+一个轮转（各512KiB），最多25组、总原始上限37.5MiB。无锁FD读取保留轮转/路径竞态、大小/mtime变化、截断和尾行完整性。快照可能包含窗前/窗后记录，按时间及进程身份过滤；源限频/事件采样和小时之间轮转会遗失细节，不能宣称完整触发或内存分配审计。

主要观察口径：

- 逐有效、同身份、无sleep/gap/unknown/clock屏障的分钟CPU区间，检查watch单核>=50%的次数、峰值、CPU秒和连续区段。所有原记录process-measured高CPU候选与source/DB一致的严格归因集合分别保留，DBunknown不能隐藏高CPU。分钟均值不能排除更短峰值。
- UserDropped需以原始recovery flags/事件ID、worker计划/边界、完成/失败/重试和revision/last_indexed_at关联；batch OR flags不等于每条原始flags，缺快照不等于未发生。不把必要恢复默认判为冗余。
- GUI/serve/watch按实际boot/PID/start cookie/UUID/executable身份分别计算单小时CPU/RSS/footprint中位/p95/峰/端点、OLS/TheilSen及尾60有效内存端点的平台。累计hourly summary不当单小时；跨小时CPU按该分钟内均匀分配估计并注明，原分钟总量保留。尾60端点不必等于60分钟；第24小时末单点不当额外完整小时；非正内存端点单列无效，不作释放。
- 睡眠/电源UUID变化/缺口/角色未知/重启分段，缺失不补齐。RSS回落不表示footprint释放；单台阶不直接认定泄漏，平台也不能排除泄漏。连续数小时趋势候选需说明区间、样本数/有效时长和普通使用上下文。

保留的历史事实：旧29b24h有4次flags3 UserDropped全根恢复、24个watch分钟区间>=50%、峰82.080153%，upstream缓冲压力和每次恢复必要性未唯一归因；旧18:48 serve footprint+47.578125MiB后平台、旧GUI多次台阶原因仍未知。更早watch9区间>=50%峰85.6349%、revision+7341204且无持久原始触发；TopK安装普通点总footprint+17.690%、上轮29b安装serve+40.844MiB/+2.717%独立保留。

当前优化受控收益和限制保持原[验收报告](../watch-memory-20261022/REPORT.md)：合成预到达drop恢复3→1、扫描CPU/写量约-67%；真实小树事件CPU-18.64%；code CPU+1.602%、短词组+0.356%在+5%门槛内。短词离散尾footprint+2.320MiB、容量固定；普通安装serve+0.968658MiB；原生首次查询关联约432MiB footprint台阶，均不能唯一解释旧47.578MiB，也不能概括整体内存下降。本新观察不是历史优化因果A/B。

自然到期后复核writer退出原因/锁和原始稳定性，再保存逐文件SHA与独立脱敏逐小时统计、异常时间线、实际wall/CPU有效覆盖/睡眠缺口及未覆盖项，提交推送收尾记录并精确远端回读；App继续固定4dc。到期不自动重开，不将窗口结束标为连续24h稳定或无泄漏PASS。
