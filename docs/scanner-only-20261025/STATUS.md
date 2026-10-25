# MacEvery 扫描优化交付与独立观察准备

2026-10-25，r3，APP_INSTALLED_NATIVE_PASS_OBSERVATION_PREPARING。

用户明确选择只采用scanner/精确seen与固定根批次优化；catch-up暂不进入App，完整候选1b21ef4、隔离原始和内存取舍保留于recovery-batch-20261025。src/server.rs及journal读取恢复b027基线路径。当前App已固定0.1.3-ge6878ef；旧窗口、异常、冻结版和所有真实回滚不改不拼。

私有根`.build/scanner-only-20261025`。计划：源范围核验→Rust debug/release/Clippy/fmt与扫描/短词/完整结果门禁→独立App原生验收→闭合新真实索引备份后App-only替换→提交推送精确回读→固定源码独立24小时唯一采样器。原生验收只用隔离副本；安装后真实增改名删为授权验收，清理fixture并恢复原模式/查询/快捷键。catch-up内存调查与性能测量串行，不与新观察重叠；新窗开始后不运行额外资源采样器或压测。

扫描源码已安装并推送，新观察尚未开始，同ID heartbeat仍PAUSED。任何失败保留原始，不把旧1b21功能/资源结果当拆分后结果。

拆分后门禁完成：212单元+8集成debug/release，Clippy/fmt/Swift；18恢复A/B/A扫描CPU约−33–36%、每批等待约+0.82秒、根数不变；29短词digest一致，code CPU+0.16%、最差+3.11%均<+5%；真实及对抗完整Top300/更新/15秒通过。见REPORT r3与evidence/isolated.json；拆分后结果独立于旧组合候选。

原生与安装已完成：固定App0.1.3-ge6878ef/source e6878efa2515e00ff35192eb29f4c6eb73a37b4b；源码已推送精确SHA回读；新真实索引回滚闭合quick_check ok，旧App原inode保留，真实库未adopt。两次原生更新均1/1/1/0。验收/tmp不收敛和Quit后UI自动重开连接真实库的事件已在REPORT r3/installation证据保留，净变更范围未知，不得宣称真实库全程零变化。新观察开始前收完全部调查/采样与原始，随后只用唯一冻结采样器。
