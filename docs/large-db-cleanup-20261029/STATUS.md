# MacEvery 历史大库清理状态

2026-10-29，r1，COMPLETED_17_EXPIRED_DBS_REMOVED_LATEST_ROLLBACK_RETAINED。

新授权下17份历史主库已逐一全SHA后删除，约83.925GiB，.build约93.903→9.980GiB，卷可用同期净增约60.088GiB。保留最近scanner-only安装真实回滚（SHA971e8889…）与b027旧App、当前e687、所有小冻结源码/App、测量日志、观察/异常原件及14份sidecar；31,353保留文件复核通过。

13个源码提交均在已刷新的本地/远端主干，84冻结source SHA能由Git重现，无待合源码。catch-up已在历史中且被e687按原决定撤回，不重新启用；本次只清数据/更新文档，不构建/请求/重启App、不动真实运行库或观察工具、不新开观察。

新过期tombstone与逐库完成回执解释原路径缺失，旧SHA和保留清单不回写。历史主库/母本/native失败库不能再逐字复跑，单独WAL/日志不能还原。见[报告](REPORT.md)、[清理摘要](evidence/cleanup.json)、[源码核对](evidence/source-audit.json)。私有`.build/large-db-cleanup-20261029`保存删除前SHA、保留核验和后续发布回执；推送须以精确远端回读为准。
