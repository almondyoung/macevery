# MacEvery 扫描优化交付

2026-10-25，r3，拆分后的隔离及原生门禁通过，App-only已替换为0.1.3-ge6878ef，源码远端精确回读通过。用户选择scanner/精确seen与固定根批次；catch-up不进入App，完整候选1b21ef4和原始证据保留。src/server.rs逐字节等于b027业务基线，journal读取不含catch-up。

212项Rust单元+8集成在debug/release均通过，Clippy/fmt与Swift回归通过。同一6003条小树两组A/B/A共18次，完整13字段/ID、未变行写0、stale删除、失败checkpoint、后边界义务通过。

| 场景 | 扫描CPU基线→候选 ms | CPU变化 | 总处理wall增加 s | 扫描数两侧 | 尾FP差 MiB |
| --- | --- | --- | --- | --- | --- |
| 扫描前积压 | 66.143→43.279 | -34.57% | 0.827 | 1/1 | -0.023 |
| 扫描后新drop | 138.459→93.376 | -32.56% | 1.652 | 2/2 | -0.047 |
| 失败重试 | 71.153→45.214 | -36.46% | 0.823 | 1/1 | -0.023 |

本次扫描CPU约降33–36%，不覆盖此前组合候选42–52%的独立结果；批等待每轮约多0.82秒，没有测出扫描数下降，也未证明OS UserDropped根因消除。短尾FP差不是长期平台或泄漏结论。

29短词完整digest匹配，code CPU +0.16%，逐项最差+3.11%均在+5%门槛内。真实739万关闭副本及3万对抗数据，各6视图×24阶段的CLI/memory/sqlite六字段完整Top300/顺序及修改/改名/删除均通过，真实与探针fuzzy均执行；所有实际候选请求≤15秒。退出后非正内存端点单列INVALID，不解释为释放。

见[脱敏证据](evidence/isolated.json)。旧数据/异常/冻结App/source及所有真实索引回滚保持。独立App原生验收、App-only备份/替换、源码提交推送均已完成；新窗启动另有独立回执。catch-up隔离65秒VM尾段调查另存，不混入扫描候选资源结果或新观察。

原生独立候选Fastest/Low Memory均code300、实际sqlite后端及用户目录fixture增改名删1/1/1/0通过；安装后Fastest code300与真实fixture1/1/1/0再次通过，1234B/旧路径清理核对。模式memory、快捷键Command-Shift-Space、空查询恢复，权限Limited保持。签名App与受测CLI通过仅私有副本签名LINKEDIT规范化的逐字节payload证据；真实DB inode/标识/roots/excludes保留。先闭合所有角色/DBFD，再新建独立真实索引备份、quick_check ok，App-only独占rename保留旧App原inode。备份普通RO验证失败原样保留，证明私有副本关闭且无sidecar后仅对该副本immutable验证成功，未对真实库使用immutable。详见[evidence/installation.json](evidence/installation.json)。

验收异常不隐藏：首次/tmp测试文件20秒未收敛，改用用户目录新fixture通过，原因未证实。一次Quit后getAX状态读取重新打开了候选且丢失隔离环境，候选短暂连接真实库，watch日志可见自然DML；已正常关闭并核对FD全部关闭。由于无该瞬间前的当前revision，净变更范围未知，未伪称真实库全程零变化，未回滚SQL。受测bundle未替换原b027；新回滚是在这段关闭后创建，历史更早回滚仍在。后续Quit不再调用会重开的UI状态读取。该事故单独保留原日志，不当新的观察窗口数据。

catch-up继续隔离调查：旧候选一组65秒尾A/B/A，同容量1538889152B，full两次reusable均237.703MiB、catch-up为0，compressed均0；65秒尾FP full中位1348.650MiB、catch-up1569.674MiB，差+221.023MiB。较高FP持续65秒，与full重建后的VM页面记账状态不同相符；不是新增218MiB分配或已证明泄漏，具体allocator/SQLite页归属仍未知。按实际身份/session/sequence、at_ms与缓存snapshot_at_ms区分，idle不重复算full。详见[evidence/catchup-memory.json](evidence/catchup-memory.json)。该代码没有进入扫描App，也不会在新窗内继续资源压测。

旧观察、异常、冻结版/source、完整候选和所有真实回滚均保留；新独立24小时观察另立watch-observation-20261025，当前文档及验收/调查原始不拼入其journal。
