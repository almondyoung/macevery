# MacEvery 扫描优化交付与独立观察准备

2026-10-25，r2，SCANNER_ONLY_ISOLATED_GATES_PASS_NATIVE_PENDING。

用户明确选择只采用scanner/精确seen与固定根批次优化；catch-up暂不进入App，完整候选1b21ef4、隔离原始和内存取舍保留于recovery-batch-20261025。src/server.rs及journal读取恢复b027基线路径。当前App仍b027；旧窗口、异常、冻结版和所有真实回滚不改不拼。

私有根`.build/scanner-only-20261025`。计划：源范围核验→Rust debug/release/Clippy/fmt与扫描/短词/完整结果门禁→独立App原生验收→闭合新真实索引备份后App-only替换→提交推送精确回读→固定源码独立24小时唯一采样器。原生验收只用隔离副本；安装后真实增改名删为授权验收，清理fixture并恢复原模式/查询/快捷键。catch-up内存调查与性能测量串行，不与新观察重叠；新窗开始后不运行额外资源采样器或压测。

尚未安装、推送或新开观察，同ID heartbeat仍PAUSED。任何失败保留原始，不把旧1b21功能/资源结果当拆分后结果。

拆分后门禁完成：212单元+8集成debug/release，Clippy/fmt/Swift；18恢复A/B/A扫描CPU约−33–36%、每批等待约+0.82秒、根数不变；29短词digest一致，code CPU+0.16%、最差+3.11%均<+5%；真实及对抗完整Top300/更新/15秒通过。见REPORT r2与evidence/isolated.json。本轮尚未替换App。
