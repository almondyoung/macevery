# MacEvery b027独立24小时观察记录

2026-10-23，r1，观察中，非最终结论。窗口2026-10-23 15:24:53.957679至2026-10-24 15:24:53.957679（Asia/Shanghai），唯一collector每60秒；首端点+0.057852083秒，此前CPU无覆盖、不补足。固定0.1.3-gb027a42/source b027a42c2cb859235b2a354c4c4bd8187a785a77。该版本的恢复/刷新/短词/GUI资源与完整正确性、真实15秒集成和原生验收记录见../recovery-refresh-20261023/REPORT.md；数据和异常/冻结/真实索引回滚保留。

重点：UserDropped逐事件与batch OR分开；watch有效同身份分钟单核>=50%候选与严格集合都保留；history_gap full_after/fallback_full、snapshot_reconcile和idle缓存摘要分开；按实际boot/PID/cookie/UUID/executable与模式屏障逐小时统计三角色CPU/RSS/footprint中位/p95/峰/端点、斜率与尾60平台。累计hourly文件不当单小时，RSS下降不当FP恢复，单台阶/平台不证明或排除泄漏。

新窗口开始后不向App发新搜索/status/control、不改App/库/配置/权限、不构建压测或改冻结观察工具；用户自然使用/模式切换/睡眠按实际记录，不干预。每小时最多一次active+一个轮转的有界日志快照，37.5MiB总原始上限，startup1024B单独只读baseline；无App writer锁，FD/轮转/尾行/限频与缺口保持。保存日志不足以证明全窗没有事件或完整分配因果。

历史未决保留：4dc窗口1轮UserDropped恢复/6个>=50%分钟峰76.03%，完整扫描7,390,140行代次写放大与19:25history_gap；GUI末80分钟+3.766MiB；旧29b4轮drop/24分钟>=50%峰82.08%与serve+47.578MiB；更早9分钟峰85.6349%/revision+7341204未持久原始触发；TopK普通安装总FP+17.690%、29b serve+40.844MiB/+2.717%、4dc普通serve+0.969MiB及短词尾FP+2.320MiB；b027普通serve+5.453MiB/+0.349%和history_gap复用原索引FP高于基线重载后均不是唯一归因或整体降低。新窗口不是历史优化因果AB。

正常无变化安静；新UserDropped、高CPU、history_gap全量回退或连续数小时增长候选/未解释长缺样/提前退出/身份失配才通知。到期必须确认window_finished + last_exit.reason=window_elapsed + exit0 + 实际原worker退出 + writer锁空闲 + 原始文件稳定，signal exit0不当自然结束。完整数据/异常时间线/逐小时/coverage与逐文件SHA独立封存，说明首端点/睡眠/缺口/未覆盖项，不能称连续24小时稳定或无泄漏PASS。完成后暂停同ID，不归档、不自动新观察。

## 启动复核

前三个committed端点/119.955秒CPU区间、实际worker/锁/单session/App/DB/三角色核验通过，无初始外sleep/gap/身份屏障。首端点RSS/FP MiB：GUI149.3125/65.7823、serve1571.1719/1567.3767、watch17.7031/5.2193；普通使用起点，非受控AB。短序列不判持续增长或平台。

首slot有界日志保留44 watch/4 serve窗内且prefix截止前可归属记录：UserDropped原始/OR/寿命累计尚无可见增长，高CPU>=50%分钟0；缺日志不证明全窗0。serve捕获1次event_limit full_after/fallback_full，history_gap full_after及snapshot_reconcile各0可见记录；两者不能混称。FP从首端点约1567.377回落至1350.861MiB，RSS反而上行，容量/allocator/VM唯一归因未闭合。后续同身份趋势继续由既有JSONL记录，不另插样本。

raw与first检查证据只保存在本轮私有根；启动SHA及约束见evidence/start.json。正常无变化安静，到期统一汇总。
