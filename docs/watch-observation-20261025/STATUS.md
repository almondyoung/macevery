# MacEvery e687部分观察封存状态

2026-10-26，r7，SEALED_PARTIAL_CLOSED_RESULTS_PUSH_VERIFIED。

原14:56:01截止已过；worker昨晚23:43:16以requested_signal_stop/exit0退出，window_finished=false，不是window_elapsed。528端点/各526 known/525有效段，31499.965994秒（36.458294%）；最后有效身份23:41:01，此后不补足、不跨重启拼接。

数据于15:31封存在`.build/watch-observation-20261025/sealed-partial-20261026`：152个清单文件/27065590字节，129来源复制并逐SHA回读；journal SHA65046c6d851e92c06aa60cee12fe01716f5d38aa110fff199539c934fbd95b99不变。原worker消失、collector空、锁空闲、committed prefix闭合，App/source与四private/two public SHA一致。见[报告](REPORT.md)、[逐小时](HOURLY.md)、[异常时间线](ANOMALIES.md)、[封存锚点](evidence/seal.json)。

一轮UserDropped与两个高CPU分钟（峰63.842%）已合并额外证据，不能被hourly-only零可见触发覆盖。serve计数未增且无新搜索，不能证明下一次不重载。GUI/serve退出前平稳，watch回落7.969MiB；末60点约59分钟，不是当前平台/无泄漏PASS。信号来源/是否升级未知，后半无覆盖。

清理9份重复工作库和10处缓存已复核，母本/冻结版/真实回滚/观察及失败保留，旧SHA不改，新tombstone解释。用户明确不继续清理。见[清理报告](../build-cleanup-20261026/REPORT.md)。

封存结果、清理审计和两份保留策略补记已提交推送origin/main，结果提交ddaf9252089cb4c174f42042dfea9fe25d5a5c67已用git ls-remote精确回读，见[推送回执](evidence/push-readback.json)。业务源码未改，App仍e687；本周个人登记已更新，不改公司人天/共享。

本轮收尾完成，不再清理/采样/补足/拼接，不等待window_finished变true。自动跟进macevery-48在最终文档回读后删除，工具回执保留于私有publication-20261026；保留聊天和全部证据，不归档。下一轮仅在用户看过汇总后另行决定，不自动开启。

旧r5和封存前报告/证据已在私有seal/docs-before-closeout原样保存，历史结论不回写。
