# MacEvery 恢复批处理与有界日志追赶

2026-10-25，r3，ISOLATED_FUNCTIONAL_CPU_GATES_PASS_MEMORY_TRADEOFF_PENDING_CHOICE。

项目：MacEvery；lane code / benchmark / report；本机隔离开发。基线源码6d9873c（业务源码b027），当前App0.1.3-gb027a42保持不变，私有根`.build/recovery-batch-20261025`。原窗口已封存，数据、异常、冻结App/source和所有真实索引回滚保留；不启动/补足/拼接任何观察，不改真实库或请求App。

用户已授权继续上一轮建议：优先UserDropped/全根恢复CPU和event_limit重载，先定位与方案，再最小实现及隔离验证；搜索结果/排序/更新、code短词和CPU/内存门禁保持。当前阶段不安装或重启App；未通过全部验证不得替换。没有开启新的24小时窗口，同ID自动跟进保持暂停。

源码事实：每个scanner未变访问仍执行no-op INSERT和ID SELECT；take_batch已有1秒硬deadline但根恢复150ms空闲就提前flush；扫描后到的新drop是新的义务，不能直接跳过。event_limit在journal读取前按4096 revision span触发，history_gap快照核对的4096变化上限无法处理本次4751唯一ID。

计划：1）scanner单次prepared未变probe及精确seen热路径；2）根批次等待原1秒上限，不改普通局部路径或吞掉边界后事件；3）保留4096普通增量，event_limit在同一WAL快照内独立有界catch-up并一次apply/publish，失败/超预算不发布部分索引。

验证：目标回归→Rust全量debug/release、fmt/clippy、Swift；完整Top300/六字段/内部13字段/ID/NULL/Unicode/外部SQL/删除复用/失败回滚与恢复checkpoint；同输入A/B/A、真实HTTP15秒完成和owned子进程清理，code等短词CPU+5%门槛。资源测量不与编译或其他测量并发，普通点/历史窗口不当因果AB。

最新验证：218 Rust单元+8集成，debug/release均通过；Clippy/fmt和Swift回归通过。独立源码复核未发现正确性阻断，已补catch-up读错误重试、16384/16385硬边界、虚拟时钟固定deadline/普通idle/close合同。第一版单元素循环Clippy失败原日志保留，已修复；没有把失败写成通过。

资源已完成：18次恢复A/B/A全字段与checkpoint通过，扫描CPU−41.72/−46.57/−51.70%，总wall每批约+0.82秒，根数两侧1/2/1；4751-ID两组A/B/A刷新CPU−94.18%，短尾FP候选较full基线+218.102MiB（自身刷新约+2.35MiB），不称整体内存改善。29短词digest一致、code CPU+0.73%/聚合+0.17%、逐项<+5%；真实及对抗Top300各六视图、实际15秒门禁通过。

下一步：让用户选择先采用scanner/恢复优化，或接受catch-up内存取舍后一起原生验收；当前App不换，不自动新观察。报告r2、evidence/isolated.json与私有完整原始/ownership/SHA为依据。OS UserDropped上游、App三角色退出原因和页面分配仍UNKNOWN；不因callback子集低耗时、位图已drop或FP平台判定根因/无泄漏。

用户2026-10-25回忆：凌晨退出“应该是系统升级/重启自动退出的”。作为用户提供的原因候选，非已验证重启；封存中collector同boot/同PID连续，不能据此确认整机重启。原UNKNOWN证据不回写，本轮不新增崩溃修复范围。

2026-10-25后续选择：用户明确只采用扫描/根批次优化，catch-up暂不进入App。完整本阶段候选1b21ef4及原始资源取舍保持；交付已由[scanner-only-20261025](../scanner-only-20261025/STATUS.md)接续。当前运行App以新任务状态为准，不再停留于本文件历史待选择状态。
