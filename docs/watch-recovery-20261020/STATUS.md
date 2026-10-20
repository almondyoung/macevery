# MacEvery监听恢复、写入与GUI资源优化

更新：2026-10-20，r4，Asia/Shanghai。Owner：当前聊天。

## 目标与范围

- 本机code/benchmark/review；仓库macevery，分支main，受测基线源码a4418a717811d036017efc028d02e036e9be97a4，当前App0.1.3-ga4418a7继续运行。
- 用户授权：先封存24小时及完整异常，再清理过期临时库；按顺序补有界触发日志、缩小不必要的监听重扫、减少写放大，同时排查GUI footprint。正确性和资源/短词门禁通过后可替换App、提交推送。
- 允许：watch/scanner/db与最小Swift修复及有针对性的测试/工具/文档；新独立证据目录 `.build/watch-recovery-20261020`。
- 禁止：直接SQL改真实files、改roots/excludes/快捷键/TCC、删冻结App/真实回滚/原始观察、用候选截断或延长15秒预算换速度、覆盖失败或自动拼接/重开观察。
- 版本：日常开发维持0.1.3并标记gSHA；仅正式提测/发布递增patch一次。

## 当前状态

- 前窗口SEALED_WITH_ANOMALIES_AND_SLEEP_GAPS：[收尾报告](../topk-consistency-20261019/REPORT.md)、[脱敏结束摘要](../topk-consistency-20261019/evidence/observation-finish.json)。worker实际PID不存在，writer锁空闲，33个raw文件及53份阶段材料复制逐字一致；旧67点独立保留。非无泄漏/连续24小时PASS。
- 私有封存 `.build/topk-consistency-20261019/sealed-24h-20261020`；原件及旧冻结8096e0a、a4418a7冻结App/source archive、真实回滚恢复图全部保留。
- 前窗口收尾已提交推送：main `c3a78778ff39077b0c82e74df85bab354d85a05e`，精确远端回读一致；文档提交未重建App。
- 清理：COMPLETE，28目录均删除，6,722文件/1.210439GiB私有归档逐一SHA独立复核通过，142条数据库指纹保留但历史大库已删除。原du127.852GiB，实测空闲+15.490784GiB，不混称。见[清理报告](HISTORY_CLEANUP.md)和私有 `.build/history-cleanup-20261020/state.local.json`。
- 清理后原始/封存raw、当前App/真实DB、冻结/回滚及旧67点再次核对保持。新轮基线App、源码archive及cached rlib已独立冻结于 `.build/watch-recovery-20261020/baseline`。
- 清理记录已提交推送：main `2c3ed5de81758f12c8f9c89fd080f122ec496426`，精确远端回读一致。清理脚本及旧观察器均不得重复运行。
- 新轮正式基线：干净源码archive与a4418a7的Rust源码树相同，独立release library已构建并记录SHA；真实索引以只读事务备份成新独立快照，quick_check及内容身份通过。旧cached rlib未作为正式基线。
- 实现/验收IN_PROGRESS：watch/scanner、db、Swift分工独立，root统一运行。Rust新增真实deferred-FK COMMIT失败回滚验证已通过；debug 170单元+8集成通过，release/Clippy/fmt另存私有日志。完整fixture和真实快照六view×24阶段的Top300/更新门禁通过，未延长15秒预算。
- 独立review两项恢复反例已修正并验证：Must与HistoryDone组合保留局部恢复；relative/alias/无字面交集的未知scope保守恢复全根。最新debug/release均172单元+8集成通过，Clippy/fmt通过。GUI完整回归与资源正确性v4通过，私有还原旧finish的真实pipe负例明确FAIL；测试autoreleasepool失败原件保留。
- 历史失败保留：首次TopK隔离HTTP受sandbox拒绝（授权执行重跑PASS）；首次新Swift测试类型推导编译失败；一次optimize_cli并行临时目录时间戳碰撞已加进程内序号后全量重跑PASS。这些与产品搜索结果失败区分。
- A/B/A已完成并独立复核：36次scope/logging小树、100k upsert、657次真实短词、6进程GUI资源；最后冻结CLI真实六view×24阶段再验PASS。code/短词聚合CPU及内存门禁通过，首轮wall尖峰、日志/普通FSEvents及GUI model局部CPU增加均保留，见[本轮报告](REPORT.md)。不宣称所有项都下降或历史footprint原因已解。
- 待闭合：固定本地源码提交、生产Swift真实15秒超时/取消、远端主干包含性/精确回读、clean候选App、App-only替换（真实索引不维护/替换）、原生搜索/更新与最终普通资源点、验收文档推送。当前App未替换，MacEvery窗口本轮已只读确认可访问，未改查询。

## 下一步

1. 前窗口封存、文档推送及过期库清理已完成，不重复。
2. 实现/隔离资源主门禁已完成；只继续上述待闭合项，保留所有负例与失败，不重复旧观察器/清理。
3. 冻结新轮可追溯基线和最小隔离DB。先有界日志：容量/频率/轮转、隐私、不自写循环，记录来源/原始flags/ID/范围/重试/完成失败；再修恢复范围，单独MustScanSubDirs限子树，真正丢事件/回绕/根变化和组合flags保留必要恢复，排除过滤不可吞恢复。
4. 减少可避免写入与prepare成本；公开indexed_at及stale cleanup兼容必须明确，不能删触发条件伪造低revision。GUI排查区分源码风险、可控重现和普通使用观测，不凭RSS推断footprint释放。
5. 先三路径完整有序Top300/更新与真实FSEvents、恢复、不完整扫描、重试/rollback/checkpoint/外部写/journal gap/清理保真，再同输入同物理布局A/B/A，code与短词CPU门槛+5%、RSS/footprint无实质增长，真实15秒超时/取消、Rust/Swift必要检查与独立review。
6. 验证通过后源码clean提交推送/远端主干包含性回读，构建标识清晰的已测App，核对签名/payload/配置/权限/进程身份，保留原件替换，原生搜索和文件增改名删验收、最终资源回读，提交推送记录并回读精确SHA。锁屏只阻断原生门禁，继续隔离工作并请求解锁，不能跳过门禁。

## 不重复

- 原始触发path/flags未记录，11点具体触发原因/该次重扫必要性未知。
- 普通安装点footprint+17.690%未解；serve末RSS6.906MiB但footprint1544.502MiB，不是整体内存降低证据。
- GUI多次阶跃与回落（全窗峰89.485、末66.032MiB）不是无泄漏证明；睡眠失效区间不计CPU。
- 本轮结束不自动创建新观察；同heartbeat跨轮按本STATUS推进，完成全部后续或明确人工阻断后才暂停，不归档聊天。
