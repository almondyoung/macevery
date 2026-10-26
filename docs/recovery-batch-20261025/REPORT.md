# MacEvery 恢复CPU与有界日志追赶候选

2026-10-26，r3，隔离正确性与CPU门禁通过；有明确的内存/等待成本取舍，当前App仍b027，未替换、未重启、未改真实索引或配置权限，未启动新观察。基线6d9873c（业务b027），旧数据/异常/冻结版与所有真实回滚均保留。

## 做了什么

- scanner未变行用prepared SELECT一次比较内容并取得真实ID，减少无效写入尝试+第二次查询；精确seen页用单次map entry。变化/新行仍执行原upsert、读取写后trigger ID，generic indexed_at合同不变。
- 根恢复用现有固定1秒批次窗口；普通局部更新仍150ms idle。扫描开始后的新drop仍属于后续恢复义务，checkpoint不越界，没有按“零写”丢弃新义务。
- event_limit保留普通4096快路，新增独立16384事件/8192唯一ID/32MiB文本/1秒合作式journal追赶；固定同一WAL target，完整读与commit后一次apply/publish，失败/超预算仍保守full或stale/invalidate，不发布部分索引。history_gap仍独立处理。

## 验证结果

218项Rust单元+8项集成在debug/release均通过，Clippy -D warnings、fmt、Swift回归通过。新增回归覆盖trigger移动ID、ABORT/rollback后复用writer、全字段/NULL/Unicode、4751级最终状态、hidden victim/删除复用、并发pin后提交、16384/16385硬界限、读错重试、虚拟时钟根窗口/普通idle/close。初次Clippy的单元素测试循环已修，原失败日志保留；调度敏感的线程计时测试已替换。

两组同输入A/B/A的6003条真实小树：

| 场景 | 扫描CPU基线→候选 ms | CPU变化 | 总处理wall基线→候选 ms | 根扫描数两侧 |
| --- | --- | --- | --- | --- |
| 扫描前已积压 | 64.353→37.503 | -41.72% | 220.591→1043.844 | 1／1 |
| 扫描后边界新drop | 124.955→66.763 | -46.57% | 435.479→2077.090 | 2／2 |
| 一次失败后重试 | 62.377→30.130 | -51.70% | 217.750→1035.531 | 1／1 |

完整13字段/ID、未变files写0、stale删除1、失败不推进checkpoint、后边界新义务均通过。测得扫描CPU降低约42–52%，不声称扫描次数减少或OS UserDropped已修。固定合并窗口使小树每批总耗时多约0.82秒；扫描CPU与含等待的processing wall分开保留，不能把CPU节省写成总响应更快。恢复尾FP分别约−0.055/+0.031/−0.039MiB，仅短尾、非平台结论。

真实739万条关闭回滚的全新/tmp APFS副本，4775 revisions影响4751 ID：4748普通metadata更新、24次alpha更新、删除/复用/新增。两组A/B/A全部六字段完整code300、探针元数据/排序、目标revision一致；基线确认为event_limit full，候选确认为journal_catchup、4751 IDs、只发布一次。

刷新HTTP wall中位数 2.858749→0.166384秒，CPU 2.835240→0.165013秒（-94.18%）；后续12次常驻code聚合CPU +1.12%。

**内存取舍尚需选择：**候选刷新阶段自身FP中位增加2.352MiB；1秒短尾FP基线1351.252、候选1569.353MiB，差+218.102MiB（+16.14%）。保留原索引与全量重建后的页面状态不同，具体VM/allocator归属未闭合；不能把差值全说成候选新增分配，也不能淡化这个时点实际更高的FP。没有做purge/压测迫使归还；一秒尾点不证明长期平台或泄漏。

短词两组A/B/A、29个查询、每进程重复两轮全部digest匹配；code CPU +0.73%，查询中位数聚合+0.17%，逐项均在既定+5%门槛内。真实大库及3万条对抗数据各6视图×24阶段结果对比，CLI/memory/sqlite完整六字段、Top300、顺序及改名/修改/删除均匹配，真实模糊及探针模糊均执行，所有候选实际请求≤15秒。

TopK CLI退出后采到的非正内存端点单列INVALID，不当释放；功能/CPU结果另验。全部owned子进程与端口回收，原源/母本SHA保持；没有同时跑两个resident索引或让编译与测量并发。所有原始阶段、失败、资源和ownership回执保存在本轮私有根，脱敏数值见[evidence/isolated.json](evidence/isolated.json)。

## 未决与下一步

当前可选择先采用scanner/恢复优化，或接受catch-up的CPU/短尾FP取舍后一并做App-only替换和原生验收。没有把资源取舍自动当作整体内存优化通过；App、原库、系统权限与暂停的观察保持，等待这一实现选择，不自动重开窗口。

UserDropped的OS底因、GUI历史增长与页面归属仍UNKNOWN。留存callback低耗时/无inbox overflow子集不够归因；seen位图在刷新返回前释放，约0.938MiB残留只可列allocator/SQLite页候选，不认定泄漏。用户2026-10-25回忆凌晨退出“应该是系统升级/重启自动退出的”，这是用户原因候选；原collector同boot/同PID连续，未确证整机重启，旧封存UNKNOWN不回写。

合作式1秒catch-up不能声称全路径硬15秒，正常实际完成≤15秒仅约束本次被测输入；没有本轮真实OS drop复现或新的24小时结果。保留b027普通安装FP+5.453MiB、历史未解台阶和旧短词/普通点负向事实，不用本轮小树/新查询序列做历史窗口因果A/B。


2026-10-26保留策略补记：按用户最新“过期文件清理”授权，只删除与所在母本实际SHA完全相同、无打开FD的重复catch-up工作主库；母本、每run WAL/SHM/copy回执/测量和全部失败/异常保持，native-attempt1/2与所有真实回滚/冻结App/source不动。精确范围及新tombstone见[清理报告](../build-cleanup-20261026/REPORT.md)。原preservation.json和SHA256.local.json不回写，仍是历史保留回执；部分旧工作副本路径现由本次保留期届满回执解释，不能再称全部原路径仍存在。旧编译缓存仅清理明确叶目录，release和冻结binary保留。当前e687观察已提前信号退出，原定14:56仅封存部分覆盖，不补采不拼接，是否新开一轮由用户汇总后决定。
