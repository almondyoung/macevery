# MacEvery e687部分观察封存结果

2026-10-26，r6，SEALED_PARTIAL_INTERRUPTED_NOT_24H_COMPLETE。固定App0.1.3-ge6878ef/source e6878efa2515e00ff35192eb29f4c6eb73a37b4b，catch-up未进入App。原计划10月25日14:56:01.038606至26日14:56:01.038606；实际原worker昨晚23:43:16信号退出，window_finished=false。原截止已过，封存核验于15:31完成，不是连续24小时完成或无泄漏PASS。

528个记录端点，三角色各526个已知端点/525段有效CPU，共31499.965994秒（约8小时45分，占原窗口36.458294%）。最后有效身份10月25日23:41:01，之后约15小时15分无角色CPU覆盖；记录止于23:43:01，末两段角色未知/电源UUID屏障另列。首端点前0.056920250秒无CPU，不补采续窗，不跨boot/新身份拼接。有效记录内无sleep/gap/clock屏障，不描述中断后睡眠；wall与awake基准不强凑差值。

留存证据确认一轮UserDropped全根恢复：20:49:44.959开始、20:52:23.650完成/checkpoint23.653，recovery flags3/ID859027178。admitted_raw/寿命user_dropped8不是8次独立OSdrop。约158.691秒遍历7388826、实际写383、跳过未变7388443，stale0，无可见失败/重试。所有process-measured >=50%候选2个，严格集合2个、排除0，一连续段，63.84203456%/62.44462285%，75.771289 CPU秒。OSdrop底因仍未唯一解释，不把必要恢复默认判冗余，不与旧窗口当受控A/B。

搜索服务留存526条同native身份/session唯一sequence，load2/success2、incremental6/5、HTTP29、search6/6未增长，全部idle；缓存snapshot14:45:43早于窗口。未见真实完成的history_gap/event_limit full_after或snapshot_reconcile操作。同时没有新搜索、后半无覆盖，不能证明下一次不重载或全窗零复发。

| 原身份进程 | FP首/采样峰/最后有效值 MiB | 退出前末60有效点趋势 |
| --- | --- | --- |
| GUI | 39.391655 / 39.516655 / 39.469757 | 中位/p95约39.485382，OLS−0.001250 MiB/h，小幅波动 |
| 搜索服务 | 1347.329887 / 1347.392387 / 1347.392387 | FP相同，OLS0，平台 |
| 监听 | 11.250504 / 21.078674 / 7.969299 | 恢复峰后回落，末60点相同，OLS0 |

末60点为10月25日22:42–23:41约59分钟，不是当前/退出后/新boot的平台。RSS与FP分开保存，未知/非正端点不解释为释放；离散峰可能漏掉短峰，平台/回落不证明无泄漏。完整CPU/RSS/FP中位/p95/峰/端点、斜率、有效时长与身份分段见[逐小时](HOURLY.md)及[数据](evidence/per-hour.json)。

原worker实际消失、collector清单空、writer锁空闲；journal连续sequence/行数/末记录与state闭合，1706825字节，SHA65046c6d851e92c06aa60cee12fe01716f5d38aa110fff199539c934fbd95b99。末点power_transition，随后23:50:16新boot已核验；精确信号发送者、角色退出原因及是否系统升级未知，不猜成App崩溃。

私有`.build/watch-observation-20261025/sealed-partial-20261026`封存152个清单文件、27065590字节（清单与回执自身另计），129个来源复制并逐SHA回读，原件原位保留。含baseline冻结App/精确source、manifest/state/journal/hourly/启动回执、hourly有界日志、20:49额外恢复、中断快照、离线合并及封存前文档。见[封存锚点](evidence/seal.json)、[最终摘要](evidence/final-summary.json)及[全部已保存异常时间线](ANOMALIES.md)。冻结analyzer只读hourly，本轮已单独合并额外恢复和中断日志，按原窗口/committed截止/实际身份过滤；源限频/抽样/轮转及watch行缺cookie/UUID边界保留，不声称完整OS触发或分配审计。

授权清理已完成9份与母本同SHA的工作库及10处缓存，目录计数138.582→94.147GiB、卷可用净增1.521GiB，不能把APFS共享的目录减少量当物理释放。母本、run WAL/SHM/copy/测量、原生失败库、冻结版/真实回滚/旧观察保留；原SHA不改，新tombstone解释到期副本，首个硬链接复核失败与精确修正保留。见[清理报告](../build-cleanup-20261026/REPORT.md)。用户已明确不继续清理。

历史负向事实不回写：b027四批drop/11高分钟峰72.208859%/两event_limit及退出缺样；4dc一轮/六高分钟峰76.03%/7390140代次写放大与history_gap、GUI末80分钟+3.766MiB；29b四轮/24分钟峰82.08%与serve+47.578MiB；更早九分钟峰85.6349%无原始触发。TopK/普通安装/短词FP上涨及GUI旧台阶未知保持。小树扫描CPU约−33–36%、每批等待约+0.82秒、code+0.16%/最差+3.11%门禁，以及未安装catch-up CPU−94%但65秒FP仍约+221MiB/reusable差异，是各自独立证据。启动前/tmp不收敛、Quit后UI自动重开连接真实DB且有自然DML、净变更范围未知的事故保留，见[扫描交付](../scanner-only-20261025/REPORT.md)与[候选取舍](../recovery-batch-20261025/REPORT.md)，未拼入本窗。

数据封存已完成，结果及清理/保留补记已提交推送origin/main，结果提交ddaf9252089cb4c174f42042dfea9fe25d5a5c67已精确远端回读，见[回执](evidence/push-readback.json)。App不随docs HEAD构建/替换/重启，未发App请求或改DB/配置/权限。封存后先汇总，再由用户决定下一轮，禁止自动开窗；收尾后删除macevery-48，保留聊天和证据。
