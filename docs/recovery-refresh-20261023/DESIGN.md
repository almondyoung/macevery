# 本轮实现合同与验证范围

2026-10-23，r2，实现和隔离回归完成，GUI集成及安装待验；不是验收报告。基线4dc3e4b，既有24小时原件与异常封存保持。

## 访问标记与内容写入

完整的refresh_path/refresh_roots采用精确seen-ID页位图，单页4096位、最多4096页（2MiB位载荷加有界map开销，稠密739万ID约0.9MiB）。同一写事务内只对真实持久字段变化做files写入；未变化行保留包含indexed_at在内的全部13字段。清理按原根/二进制路径边界分批读取，关闭读cursor后仅删除未见ID；成员关系是精确集合，不用概率跳过删除。

这是scanner刷新内部代次合同的明确修订：未变行indexed_at保留最后内容变更的观测代次，不再表示每次完整刷新都访问过。CLI/HTTP搜索原六字段和排序不依赖indexed_at；Rust FileRecord仍返回实际持久字段，通用upsert和外部SQL中indexed_at变化仍推进revision并可被增量观察。显式index_paths初始化/重建和保守legacy重试不强行更改旧合同。不能宣称每种扫描都消除了写放大。

若页预算超限或扫描不完整，整个fast事务先rollback，再走原legacy代次流程；真正SQLite错误仍报错/rollback，不吞错。watch既有deferred-cleanup或full_recovery_generation债务要求后续恢复用legacy，直到代次清理完成，防止把未改行当stale误删。稀疏ID和部分失败可能产生额外一次扫描尝试，需单列该成本，不把它当无回退全局收益。

## history_gap的有界完整快照核对

只对同一数据库身份的packed backend且增量返回history_gap尝试。在同一WAL读快照下按ID合并现有packed记录和SQLite流，比较全部文本/NULL回退与6个metadata字段（包括indexed_at）；暂存最多4096变更ID和32MiB文本负载，不修改正在使用的索引。

只有完整核对并commit成功后才复用原apply_changes预检/原子发布目标stamp。扫描上限1000万行与协作墙钟上限6秒；初始2秒在737万行封闭快照超时并回退，额外成本和失败原件保留。6秒版两组A/B/A完成约2.58秒核对并成功采用；并不是硬15秒总超时保证。超预算、非单调新ID、碎片化、身份不匹配等保留旧full fallback；apply意外失败invalidate，不能发布部分同步或忽略删除。旧backend的索引容量能保留，但不是绕过完整比较，也不等于查询CPU必然下降。

日常小差异仍走原变化journal，不增加后台轮询或常驻第二份索引。扫描纯代次写减少后，revision间隔很大也可能只有少量真实最终差异；若仍有数百万实际字段变化，当前有界方案可以回退全量加载。提高历史保留、跳过变更或只比较hash不作为正确性替代。

## GUI与UserDropped

GUI每查询代使用线程安全取消标记，在HTTP成功体解码前、后及主线程发布前检查；busy重试仅保留一个可取消弱引用槽位。15秒请求/CLI预算及有限backpressure重试次数不变。已开始的解码仍可能完成；减少过期工作不等于已证明旧GUI增长底因。

UserDropped底因仍UNKNOWN。补齐原始逐事件flag计数与匿名身份/启动参数摘要，保持callback无IO、诊断有界与失败不阻塞索引；不把batch OR、事件抽样、累计计数或缺记录当完整OS触发审计。具体诊断实现和测试结果以最终报告为准。

## 门禁

1. 先窄正确性，再Rust debug/release全量、Clippy/fmt、Swift回归。
2. 完整有序Top300/六字段跨CLI、memory、sqlite；内部13字段/ID/NULL/Unicode/删除复用、外部SQL、事务失败、部分扫描/排除、retry/checkpoint、15秒超时/取消。
3. 同输入顺序A/B/A恢复与刷新CPU/RSS/FP；code等短词沿用+5% CPU门槛，保留单项回退/冷暖页/离散采样限制。独立功能编译/测试不与计时测量并发。
4. 验证通过后才冻结干净候选，确认源码远端主干包含，构建到独立App目录；可逆App-only替换、原生双模式code/真实文件增改名删及配置身份回读。原真实索引/配置不作开发负载。

本轮不自动新建24小时观察、不修改系统权限、不清理旧证据。任何未运行、回退或无法唯一归因的项目均如实保留。
