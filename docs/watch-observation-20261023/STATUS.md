# MacEvery b027新版独立24小时观察

2026-10-24，r8，`SEALED_WITH_USERDROPPED_HIGH_CPU_AND_ROLE_COVERAGE_LOSS_NOT_STABILITY_PASS`。原窗口自然到期，数据和全部已识别异常已独立封存；提交/远端回读及停止跟进回执在封存后独立留存。不要再运行collector或开启新窗。

固定App 0.1.3-gb027a42 / source b027a42c2cb859235b2a354c4c4bd8187a785a77，不随docs HEAD构建/替换/重启。观察与收尾期间未主动请求App/search/status/control，未改DB/配置/权限/冻结工具。原GUI/serve/watch于10月24日02:05:54.517至02:06:54.522之间停止可见，退出底因UNKNOWN；没有自动重启。旧窗口/67点/冻结App与精确源码/所有真实索引回滚全部保留原位，不清理或重建大库。

原窗口2026-10-23 15:24:53.957679至2026-10-24 15:24:53.957679（Asia/Shanghai），86400秒，每60秒唯一采样器。最终window_finished=true、last_exit.reason=window_elapsed且exit0，实际原worker消失、collector清单空、writer锁空闲、原始文件稳定；不是signal exit0。1277条/2952152字节，连续sequence/完整末记录/单session核验通过，raw SHA `669f182a3770a26b3a9684a772e16220bd4ee41ea1cb26014435d4b624364fdf`。四private/two public工具与App/source/archive固定SHA全部match，DB pin各1277条match。

实际覆盖：每角色539已知端点，533严格CPU区间/31979.188909秒（8小时52分59秒），后738端点三角色missing约13小时18分，不能计零CPU/FP或推定无复发。首端点前0.057852083秒无CPU，末端continuous容差+0.035020458秒保留不当第25完整小时。10个sleep/gap区间合计sleep差10071.467897秒；5个power屏障不当独立睡眠数；缺164名义slot不补，日志缺槽3/4/10–16不补。wall/continuous跨度、每小时有效时长、全部屏障及身份段见evidence/coverage.json和hourly/CPU材料。

复发：两段时段共四批UserDropped根恢复，寿命callback带标志记录0→38（非38个独立OSdrop或丢文件数）；11个全部process-measured高CPU候选全部满足严格归因，3段、峰72.208859%，高段CPU450.896225秒/awake660.016787秒。四批indexed访问合计29581850，写677/跳过未变29581173，删除/警告/legacy回退0；新的drop在首批期间到达、recovery ID不同，后批零写不能判不必要重复。未变代次DML已跳过，全根遍历CPU仍在，OSdrop/物理WAL写放大及页面归属未知。

加载：可见event_limit完成全量加载2次，history_gap成功full_after与snapshot_reconcile均0可见；含启动load/success3/3、incremental_attempts/applies4/2、queries8/8。按实际serve身份/session/sequence核对计数连续性，没有仅idle缓存增长的未闭合加载候选；cached method/phase不重复计数，日志不完整仍不能证明未覆盖部分0次。

FP：退出前GUI59.297974、serve1578.220512、watch7.813049MiB。GUI尾60端点约59分钟微降0.03125MiB；serve相同尾段完全持平；watch第二轮恢复后57点/约56分钟保持7.813049，仍比事前高0.9375MiB。此前GUI睡眠后台阶回落，serve重载/VM关联台阶、watch恢复峰值与残留都保留。当前三角色FP未知，尾值是历史，不外推至后半窗；平台/单台阶/RSS回落不证明或排除泄漏。

私有根：`/Users/yangtao/pine/code/macevery/.build/watch-observation-20261023`；封存`final-archive-20261024`，230原件副本/34875751字节，含四额外诊断和冻结基线；派生/逐文件SHA另存。未把旧窗口拼入。最终阅读入口：[REPORT.md](REPORT.md)、[机器摘要](evidence/final.json)、[覆盖](evidence/coverage.json)、[时间线](evidence/anomaly-timeline.json)、[封存索引](evidence/seal.json)。原始私有数据不推送，提交脱敏文档/证据；本周登记保留个人项目归属，不计PFT人天。

收尾后停止本轮同ID自动跟进，保留证据与聊天，不自动新观察。若有残留收尾事项，先核对私有closeout回执和git ls-remote，不重启App、改库、再次采样、修改冻结工具或重开窗口。后续修复需用户新授权。历史未知原因、普通安装内存上升、旧失败/负向事实以REPORT原记录为准，不用本窗做历史因果AB。
