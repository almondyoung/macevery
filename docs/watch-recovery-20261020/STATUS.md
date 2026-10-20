# MacEvery监听恢复、写入与GUI资源优化

更新：2026-10-20，r5，Asia/Shanghai。Owner：当前聊天。本机限定COMPLETE；最终文档远端闭合以本节规定的精确回读凭据为准。

## 目标与范围

- 本机code/benchmark/review；仓库macevery，分支main，受测基线源码a4418a717811d036017efc028d02e036e9be97a4；本轮已安装clean App0.1.3-g29b6140，源码29b6140575cdd69c965732b943dccda77e8b586e。
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
- 实现/本机验收COMPLETE：watch/scanner、db、Swift分工独立，root统一运行。Rust新增真实deferred-FK COMMIT失败回滚验证已通过；最新debug/release均172单元+8集成通过，Clippy/fmt通过。完整fixture和真实快照六view×24阶段的Top300/更新门禁通过，未延长15秒预算。
- 独立review两项恢复反例已修正并验证：Must与HistoryDone组合保留局部恢复；relative/alias/无字面交集的未知scope保守恢复全根。最新debug/release均172单元+8集成通过，Clippy/fmt通过。GUI完整回归与资源正确性v4通过，私有还原旧finish的真实pipe负例明确FAIL；测试autoreleasepool失败原件保留。
- 历史失败保留：首次TopK隔离HTTP受sandbox拒绝（授权执行重跑PASS）；首次新Swift测试类型推导编译失败；一次optimize_cli并行临时目录时间戳碰撞已加进程内序号后全量重跑PASS。这些与产品搜索结果失败区分。
- A/B/A已完成并独立复核：36次scope/logging小树、100k upsert、657次真实短词、6进程GUI资源；最后冻结CLI真实六view×24阶段再验PASS。code/短词聚合CPU及内存门禁通过，首轮wall尖峰、日志/普通FSEvents及GUI model局部CPU增加均保留，见[本轮报告](REPORT.md)。不宣称所有项都下降或历史footprint原因已解。
- 源码已提交推送：29b6140575cdd69c965732b943dccda77e8b586e，远端main精确回读一致。生产Swift真实隔离7项PASS，超时后CLI成功/取消两用例的传输等待分别15.0725659167/15.0701857917秒，CLI耗时另计；完整六字段Top300顺序一致，过期回填0。
- 安装PASS：clean App0.1.3-g29b6140签名及normalized CLI与精确已测候选匹配，已App-only替换；原App改名保留且inode保持，真实DB inode/database_id/roots/excludes保持。独立5,034,500,096字节原索引备份、旧冻结App与回滚保留，未整理/替换真实索引。
- 安全失败保留：普通RO备份quick_check无法打开文件的日志保留；仅关闭且无sidecar的静态备份immutable复核PASS，never live。第一次缺有效backup receipt的替换安全拒绝，第二次核验齐全后替换PASS。
- 原生PASS：Fastest/code300、Low Memory/code300后恢复Fastest；真实fixture create22B/modify53B/rename/delete的文件系统→DB收敛等待约0.41645/0.52438/0.21508/0.21211秒，可见结果1/1/1/0，已清fixture及DB记录并恢复原查询、模式、快捷键和偏好。原生仅可见行/footer300，非全部300条AX全字段或逐帧UI延迟基准；完整比较属于隔离层。Limited→Unknown标签变化保留，未改TCC、不等同已证明权限丢失。
- 最终runtime PASS：身份/start/UUID/boot、App指纹、配置、偏好、fixture全清及33份原始/封存raw重哈希闭合；Low Memory→Fastest使serve按设计更换身份，GUI/watch保持。日志54条（17 filter/14 start/14 complete/9 checkpoint）、40,085字节/0600/单条≤2KiB，无明文home或marker、I/O failure最大0；不是洪峰长稳验证。
- 普通约20秒资源点已保存：GUI CPU0.00212456→0%、RSS142.609→153.266MiB、footprint66.423→66.282MiB；serve CPU0.00112436→0.00080630%、RSS1551.844→1547.781MiB、footprint1503.111→1543.955MiB（+40.844MiB/+2.717%）；watch CPU0.0284911→0.0191643%、RSS23.656→8.219MiB、footprint11.001→3.250MiB。使用历史/模式切换/寿命不同，不能归因或宣称整体内存下降，见[报告r2](REPORT.md)及[最终验收摘要](evidence/final-acceptance.json)。
- 最终文档随本次提交保存；远端闭合凭据为私有 `.build/watch-recovery-20261020/closeout-push-readback.local.json`，必须记录包含本次文档的本地HEAD与远端main精确SHA相同且工作区干净。该凭据缺失或不匹配时，不得视作推送完成。App保持源码29b6140，不随docs HEAD变化重建。

## 下一步

1. 查验上述最终文档精确回读凭据；闭合后暂停同ID heartbeat，保留证据、不归档聊天、不重建已测App。若凭据缺失，仅续办提交/推送/回读，不重复安装或负载。
2. 保留前窗口异常/睡眠缺口、清理归档、原件/回滚、受控比较及全部失败。已完成的清理、旧观察器和安装动作不重复运行。
3. 不自动创建新观察、不拼接或延长旧窗口；剩余长期洪峰/无界队列、历史GUI台阶与权限标签原因继续如实列为边界，不把本机限定完成扩展成长期稳定或无泄漏PASS。

## 不重复

- 原始触发path/flags未记录，11点具体触发原因/该次重扫必要性未知。
- 普通安装点footprint+17.690%未解；serve末RSS6.906MiB但footprint1544.502MiB，不是整体内存降低证据。
- GUI多次阶跃与回落（全窗峰89.485、末66.032MiB）不是无泄漏证明；睡眠失效区间不计CPU。
- 本轮结束不自动创建新观察；同heartbeat跨轮按本STATUS推进，完成全部后续或明确人工阻断后才暂停，不归档聊天。
