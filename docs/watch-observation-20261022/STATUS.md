# MacEvery 4dc 独立24小时观察

2026-10-23，r2，**NATURALLY_ENDED_SEALED_WITH_ANOMALIES**。原窗口已结束；不恢复、补足、延长、重开、拼旧数据或新增资源采样/搜索。结束不是连续24小时稳定或无泄漏PASS。

固定App **0.1.3-g4dc3e4b**，源码 **4dc3e4b5d05136ecb75ca72eaa6dc3f6ce18b114**，不随文档HEAD重建、重启或替换。原窗口北京时间2026-10-22 13:22:00.017045至23日13:22:00.017045。唯一采样器自然window_elapsed/exit0，原worker实际PID不存在、collector盘点空、writer锁空闲，35个状态原文件两次稳定核验。

1441行／4660148B，连续sequence/末行/单session/source/DB pin及三角色各单一实际身份匹配。实际wall端点跨度86400.435570秒，有效CPU awake86399.971181458秒；首0.052540958秒无CPU，终界跨出0.023725083秒保留，裁窗有效时长仅均匀分配估计。仅初始屏障，无检出sleep/gap/unknown/身份屏障。App模式未完整独立记录，mode=run只是collector模式；冻结analyzer误名wall字段实为continuous差，报告另列真实wall，不改helper。

11:01确认一轮原始flags3 UserDropped关联全根恢复，7390140行写入，6个有效分钟≥50%，峰76.027560%，249.815049CPU秒。未确认重复全根恢复缺陷；新indexed_at代次导致元数据未变行也写入/revision的机制已确认，当前stale/API语义不能直接忽略。上游drop原因、实际空写比例/物理IO未知。

末FP MiB：GUI **55.626099**，12:02以来80分钟+3.765625，尾60点OLS+1.390435MiB/h，临近终点放缓但未确认多小时平台；serve **1468.954887**、watch **8.563095**尾60点均平台。watch较恢复前仍+2.187546MiB，来源未唯一归因。19:25一次history_gap加载、arena容量和GUI关联、20:36服务VM台阶及21:53真实FP回落均纳入终报。

详见[REPORT](REPORT.md)、[最终证据](evidence/final.json)、[逐小时](evidence/hourly.json)、[异常线](evidence/anomaly-timeline.json)、[封存索引](evidence/seal.json)。小时诊断缺slot3/9/11/13/16/23未补，不等于资源缺样；源限频/抽样/轮转和窗后行保留，不称完整触发审计。

私有根`.build/watch-observation-20261022`内原始manifest/state/journal/hourly/launch/诊断/收据/调查/阶段/独立复核封存并逐文件SHA核验。当前冻结App/source archive、验收/普通点/异常、真实索引回滚、所有旧冻结/旧窗口保留；不重建/再清理大库。私有原件不推送。

收尾提交推送本轮脱敏报告/证据并精确远端SHA回读，同步本周登记；完成后暂停同ID **macevery-48**，保留聊天与证据，不自动新开任务或观察。推送/暂停外部回执另存私有收尾记录，App保持4dc。后续代码优化、新验收或替换App按用户新的任务授权推进。
