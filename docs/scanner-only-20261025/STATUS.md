# MacEvery 扫描优化交付与独立观察准备

2026-10-26，r5，DELIVERED_OBSERVATION_INTERRUPTED_CLEANUP_AUDITED。

用户明确选择只采用scanner/精确seen与固定根批次优化；catch-up暂不进入App，完整候选1b21ef4、隔离原始和内存取舍保留于recovery-batch-20261025。src/server.rs及journal读取恢复b027基线路径。当前App已固定0.1.3-ge6878ef；旧窗口、异常、冻结版和所有真实回滚不改不拼。

私有根`.build/scanner-only-20261025`。计划：源范围核验→Rust debug/release/Clippy/fmt与扫描/短词/完整结果门禁→独立App原生验收→闭合新真实索引备份后App-only替换→提交推送精确回读→固定源码独立24小时唯一采样器。原生验收只用隔离副本；安装后真实增改名删为授权验收，清理fixture并恢复原模式/查询/快捷键。catch-up内存调查与性能测量串行，不与新观察重叠；新窗开始后不运行额外资源采样器或压测。

扫描源码已安装并推送，新独立观察已启动，同ID heartbeat ACTIVE。任何失败保留原始，不把旧1b21功能/资源结果当拆分后结果。

拆分后门禁完成：212单元+8集成debug/release，Clippy/fmt/Swift；18恢复A/B/A扫描CPU约−33–36%、每批等待约+0.82秒、根数不变；29短词digest一致，code CPU+0.16%、最差+3.11%均<+5%；真实及对抗完整Top300/更新/15秒通过。见REPORT r3与evidence/isolated.json；拆分后结果独立于旧组合候选。

原生与安装已完成：固定App0.1.3-ge6878ef/source e6878efa2515e00ff35192eb29f4c6eb73a37b4b；源码已推送精确SHA回读；新真实索引回滚闭合quick_check ok，旧App原inode保留，真实库未adopt。两次原生更新均1/1/1/0。验收/tmp不收敛和Quit后UI自动重开连接真实库的事件已在REPORT r3/installation证据保留，净变更范围未知，不得宣称真实库全程零变化。新观察开始前收完全部调查/采样与原始，随后只用唯一冻结采样器。

新的独立观察已于2026-10-25 14:56:01.038606开始，至2026-10-26 14:56:01.038606（Asia/Shanghai）自然到期，按[观察STATUS](../watch-observation-20261025/STATUS.md)接续。固定e687，不随docs HEAD重建；本轮CPU/内存调查和验收数据不拼入新窗，正常无变化安静，到期统一汇总。


2026-10-26保留策略补记：按用户最新“过期文件清理”授权，只删除与所在母本实际SHA完全相同、无打开FD的重复catch-up工作主库；母本、每run WAL/SHM/copy回执/测量和全部失败/异常保持，native-attempt1/2与所有真实回滚/冻结App/source不动。精确范围及新tombstone见[清理报告](../build-cleanup-20261026/REPORT.md)。原preservation.json和SHA256.local.json不回写，仍是历史保留回执；部分旧工作副本路径现由本次保留期届满回执解释，不能再称全部原路径仍存在。旧编译缓存仅清理明确叶目录，release和冻结binary保留。当前e687观察已提前信号退出，原定14:56仅封存部分覆盖，不补采不拼接，是否新开一轮由用户汇总后决定。
