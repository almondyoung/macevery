# e687部分窗口异常时间线

2026-10-26，r1。记录本轮已保存证据中的全部异常与覆盖边界，非完整OS事件审计。时间Asia/Shanghai；限频、抽样、轮转丢失边界保留，窗后新boot日志按原身份/时间排除。

| 时间 | 事件 | 证据与边界 |
| --- | --- | --- |
| 10-25 14:56:01 | 首端点 | 普通基线；此前0.056920250秒无CPU覆盖，不补窗 |
| 10-25 20:49:44.959 | UserDropped全根恢复开始 | 实际recovery flags3/ID859027178；admitted_raw/寿命user_dropped8不是8个独立OSdrop或丢文件数 |
| 10-25 20:50–20:51 | 高CPU | 63.84203456%，38.305839208 CPU秒/60.00096875 awake秒；严格集合与全候选均保留 |
| 10-25 20:51–20:52 | 高CPU | 62.44462285%，37.465449625 CPU秒/59.997879583 awake秒；连续一段，revision增量0 |
| 10-25 20:52:23.650/23.653 | 完成/checkpoint | 约158.691秒；遍历7388826、写383、跳过未变7388443、stale0，无可见失败/重试；20:53端点revision+383 |
| 10-25 20:52–20:53 | 监听FP回落 | 采样21.078674→11.750549MiB；分配归属原因未知 |
| 10-25 21:31、21:34 | 监听FP继续回落 | 11.766174→9.313049→7.938049MiB，末60有效点约7.969299MiB |
| 10-25 23:41:01 | 最后有效角色端点 | 各526点/525段、31499.965994秒有效CPU；此后不声称平台 |
| 10-25 23:42:01 | 角色不可验证 | GUI/watch missing，serve因GUI父归属unknown；CPU无覆盖 |
| 10-25 23:43:01 | 全部missing与电源屏障 | power_transition不是实际睡眠时长证明 |
| 10-25 23:43:16 | collector信号退出 | requested_signal_stop/exit0，window_finished=false，不是window_elapsed |
| 10-25 23:50:16 | 之后系统boot | boot变化已核验；信号发送者/角色退出原因/是否升级未闭合，不拼窗 |
| 10-26 14:56:01 | 原定截止 | 剩余覆盖未获得，不重启补采，以部分中断窗口封存 |
| 10-26 15:31:07 | 实际封存核验 | 152个清单文件SHA回读，原件原位保留 |

保存的serve日志合并后526条唯一sequence全部idle，native PID/cookie/UUID/session一致；load2/success2、incremental6/5、HTTP29、search6/6从首至末未增长，缓存snapshot14:45:43早于本窗。未见真实完成的history_gap/event_limit full_after或snapshot_reconcile；同时没有新搜索，不能验证下一次查询或把后半缺样当零复发。

参见[结构化时间线](evidence/anomaly-timeline.json)、[恢复证据](evidence/incident-20261025-204944.json)、[中断](evidence/interruption-20261026.json)及[最终摘要](evidence/final-summary.json)。
