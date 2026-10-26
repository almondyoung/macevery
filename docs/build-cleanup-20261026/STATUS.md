# MacEvery .build清理状态

2026-10-26，r2，CLEANUP_COMPLETED_RETAINED_EVIDENCE_VERIFIED_RESULTS_PUSHED。

用户授权的过期文件清理已按9份重复测试主库+10处编译缓存精确执行；无新采样/搜索/构建/重启，不补采、不拼窗。母本、sidecar、全部异常和真实回滚/冻结App/source原位保留。原历史SHA清单不改，删除副本由新tombstone解释，首次硬链接验证失败及修正核对完整留存。见[报告](REPORT.md)及[逐项公开摘要](evidence/cleanup.json)，私有逐文件/preflight/completed回执在.build/cleanup-20261026。

本清理记录与保留策略补记已随结果提交ddaf9252089cb4c174f42042dfea9fe25d5a5c67推送并精确远端回读，观察部分数据亦已封存。不要继续清理、重建历史库或自动开启下一轮。观察原始83文件与journal不能改变；本次清理仅在观察根之外。
