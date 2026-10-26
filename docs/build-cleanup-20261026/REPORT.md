# MacEvery .build过期产物清理

2026-10-26，r2，EXACT_ALLOWLIST_CLEANUP_COMPLETED_RETAINED_CONTENT_VERIFIED。用户新授权过期产物清理，同时保留冻结版、真实索引回滚、观察数据和全部异常；清理没有启动App请求、构建、采样或下一轮观察。

目录du计数从138.582降至94.147 GiB，减少44.435 GiB；配对观测的卷可用空间净增1.521 GiB。大测试库使用APFS clone，共享extent；保留debug/deps对象也会继续占用缓存硬链接的数据。不能把目录减少量当作实际释放量，卷净变化还包括同期系统写入/快照。原始字节数见[审计摘要](evidence/cleanup.json)。

删除的是9份隔离catch-up A/B/A工作库和10处明确编译缓存。9份主库各5117849600字节，实际重新计算SHA均为24f6965b216af024a2f95383601e9c1ed725ca6243bf8aeffb19bfc5b41183f8，等于所在场景保留的mother.sqlite，copy.local.json亦确认APFS clone来源。每轮WAL/SHM、copy回执、原始日志/结果/请求/样本保持，原路径旁新增index.sqlite.expired-20261026.json标明母本与原SHA；没有自动恢复或执行SQL。

| 范围 | 清理 | 保留 |
| --- | --- | --- |
| recovery-batch-20261025 | catchup-aba六份run/index.sqlite | 两组A/B/A全部测量、母本、frozen-input、候选1b21源码/二进制、WAL/SHM、失败及原SHA |
| scanner-only-20261025 | catchup-memory-tail三份run/index.sqlite | 65秒VM/CPU/内存负向证据、母本、native-attempt1/2原库与失败、安装回滚、固定e687 |
| 编译缓存 | 四处Swift module-cache、六处Rust debug/incremental | release/冻结binary、debug/deps与独立验收程序、源码和测试日志 |
| performance-round1至7及其他历史轮次 | 仅上述精确缓存叶目录 | snapshot/live-index-before等真实备份、旧App/source、所有观察/异常与封存SHA |

清理前重新核对无活动编译器、九个工作库及十处缓存无打开FD；逐文件清单记入私有cleanup-20261026/preflight.local.json。清理后43396个保留文件完成核对：64MiB以下重算内容SHA，保留的大库核对精确device/inode/size/mtime/mode/owner，两个母本完整SHA重算。首次事后检查因要求st_nlink完全不变停下，原失败保留；随后按清理前cache inode清单精确解释9327个保留路径的链接数减少，内容SHA及其余身份属性均匹配，未追加删除。

旧private SHA256.local.json、raw-preservation.local.json与public preservation.json保留原字节，继续代表当时保留事实；本次新回执明确9个重复工作副本已过保留期，不能继续宣称每个旧路径仍完整存在。母本与每run sidecar/测量证据可供将来授权的复核，缓存可重建；本轮没有重建。

本轮独立观察已提前中断，不把退出后的新boot或新进程拼入。528条部分观察、20:49 UserDropped/两个高CPU分钟及中断原件完整保留，[观察STATUS](../watch-observation-20261025/STATUS.md)仍按原2026-10-26 14:56:01.038606统一封存；封存后先汇总，再由用户决定下一轮，不自动开始。

本清理记录及保留策略补记已随结果提交ddaf9252089cb4c174f42042dfea9fe25d5a5c67推送并精确远端回读；仅文档，不修改业务源码或重建App。
