# MacEvery e687部分观察封存状态

2026-10-26，r6，SEALED_PARTIAL_DATA_COMPLETE_DOCS_PUSH_PENDING。

原14:56:01截止已过；worker昨晚23:43:16以requested_signal_stop/exit0退出，window_finished=false，不是window_elapsed。528端点/各526 known/525有效段，31499.965994秒（36.458294%）；最后有效身份23:41:01，此后不补足、不跨重启拼接。

数据于15:31封存在`.build/watch-observation-20261025/sealed-partial-20261026`：152个清单文件/27065590字节，129来源复制并逐SHA回读；journal SHA65046c6d851e92c06aa60cee12fe01716f5d38aa110fff199539c934fbd95b99不变。原worker消失、collector空、锁空闲、committed prefix闭合，App/source与四private/two public SHA一致。见[报告](REPORT.md)、[逐小时](HOURLY.md)、[异常时间线](ANOMALIES.md)、[封存锚点](evidence/seal.json)。

一轮UserDropped与两个高CPU分钟（峰63.842%）已合并额外证据，不能被hourly-only零可见触发覆盖。serve计数未增且无新搜索，不能证明下一次不重载。GUI/serve退出前平稳，watch回落7.969MiB；末60点约59分钟，不是当前平台/无泄漏PASS。信号来源/是否升级未知，后半无覆盖。

清理9份重复工作库和10处缓存已复核，母本/冻结版/真实回滚/观察及失败保留，旧SHA不改，新tombstone解释。用户明确不继续清理。见[清理报告](../build-cleanup-20261026/REPORT.md)。

待完成：仅提交推送本轮观察/清理/保留策略文档并git ls-remote精确回读，更新本周个人文档登记，不改公司人天或共享。随后删除macevery-48并汇总五项结果。保留聊天和证据，不归档、不采样、不重启/替换/构建、不改DB/配置/权限、不发App请求；下一轮由用户决定，不自动开启。

旧r5和封存前报告/证据已在私有seal/docs-before-closeout原样保存，历史结论不回写。
