# MacEvery 恢复写放大与history_gap优化

2026-10-23，r1，隔离验收完成，GUI真实传输集成及App替换待验。基线4dc3e4b；当前运行App、真实库、模式、权限仍未改动。原24小时数据/完整异常/329文件封存、旧窗口与冻结App/源码、真实索引回滚保持，未拼接、未清理、未新开观察。

## 改了什么与合同

完整refresh扫描用精确有界seen-ID替代每访问一行就写indexed_at；内容完全不变的13字段和ID保留，真正变更仍写revision/journal，未见ID仍按根路径边界删除。generic upsert、外部SQL和index_paths保持原合同。部分warning/稀疏seen超预算先rollback再legacy，旧generation清理债务强制legacy，不把失败事务当成功扫描。详见DESIGN.md。

history_gap用同一WAL快照流式核对全部记录，再原子apply少量差异；最多4096 IDs/32MiB暂存/1000万行/6秒协作预算。完整核对失败/超预算、非单调新ID、碎片化等仍全量回退；不发布部分索引，不跳过字段/删除，不把扫描称O(变更量)。6秒并非硬15秒最坏延迟保证，持续写入/冷盘/大变更仍需回退。

GUI在HTTP成功体解码前后检查取消，重试改成一个可取消weak槽；保留15秒请求/CLI和有限重试预算。已开始解码仍可能完成，不能据此宣称旧末段内存增长已定位。watch新增逐事件flag计数、匿名session及独立私有启动摘要；batch OR与原始逐事件计数分开，不做callback IO。启动摘要仅configured，非成功start证明；诊断限频/旋转/崩溃/2048字节行抑制仍可能缺失，非lossless审计。

## 已完成的验证

Rust debug/release各210单元+8集成通过；scanner24/DB53/watch46/server25/compact27窄回归、Swift取消/超时预算/重试生命周期单元通过；Clippy最终通过，首轮5个测试写法提示及修正记录保留。数据包括30000行对抗fixture和旧回滚新克隆的737万行封闭快照，原5GB回滚未SQL打开/重hash，真实运行库未作开发负载。

两个数据集的CLI/memory/sqlite六条路径完整有序Top300/六字段、Unicode/过滤/改名/修改/删除一致，全部候选查询在15秒内；逐查询原件保留，不以count相等代替结果相等。

| 受控工作负载 | 4dc | 候选 | 边界 |
| --- | --- | --- | --- |
| 恢复单次6003行完整小树，扫描CPU中位 | 100.114ms | 59.598ms，约-40.47% | 两组A/B/A，模拟恢复标志，非真实OS drop复现 |
| 未变行files写入 | 6003 | 0 | 两版仍扫描6003行、删同一stale；候选13字段全保留 |
| 小树processing采样峰FP中位 | 10.110MiB | 6.125MiB | 20ms离散峰下界，非全盘外推 |
| 737万行稀疏history_gap刷新CPU中位 | 2.876646s | 2.546905s，-11.46% | 20003 revision、最终3 IDs，两组A/B/A |
| 同刷新wall中位 | 2.895338s | 2.581430s | 温缓存、HTTP含code300，非最坏保证 |
| 同刷新FP增量中位 | -217.258MiB | +0.15625MiB | 候选采样峰另+约8.5MiB；不能称整体FP下降 |
| 短词组CPU | 6063.068ms | 6092.275ms，+0.48% | 1–7字节/Unicode/broad/nohit，有序Top300一致 |
| code CPU | 155.624ms | 157.116ms，+0.96% | 所有单项均未超+5%门槛 |

18次恢复运行覆盖原有积压、边界后新drop及扫描前失败重试，checkpoint/stale与清理闭合。warning、稀疏预算及旧清理债务由功能回归覆盖，未作同输入资源A/B；不能把返回indexed当rollback重试两次总FS访问量。

初版2秒核对在大库回退，CPU5.094s对3.025s，失败证据和r1制品保留；改6秒后重验。恢复driver初次E0603/E0624是跨crate测试架构错误，后改为同一driver编译原样db/sqlite/scanner/watch，18次通过；不归为产品失败。最终driver只再修正输入SHA来源为捕获bytes，本次复制件已独立逐字节核对，未重跑资源。GUI资源工具首次传错基线SHA被预检拒绝，未执行负载，原件保留。

## 原因与未决项

旧全根恢复7,390,140次代次写推进revision，及其触发history_gap全量重载的机制已闭合；本轮针对该写放大和恢复手段。恢复必须完整核对丢失事件范围，不能把必要恢复默认判冗余，也不能把本轮9000个模拟标志当真实OS drop数量。

UserDropped上游底因仍UNKNOWN。GUI末80分钟+3.766MiB、watch恢复后残余、旧serve台阶及历史总FP增长均未获得唯一分配归因。候选保留原索引的FP末值高于基线重建后，容量/allocator/OS accounting尚未闭合，RSS较低不表示FP恢复。新诊断为后续定位提供证据，不证明已根治或无泄漏。本轮不自动新开24小时窗口。

资源与正确性脱敏数值见evidence/isolated.json；完整响应、采样、失败、源码/rlib/driver和原始SHA留在本轮私有根及owned tmp。GUI同生产formatter的两组A/B/A也已通过，正常300结果/clear及offscreen工作量一致；寿命CPU配对+2.17%、model阶段+1.26%、idle-after FP约+0.31%，没有证明GUI整体更省或旧增长原因。formatter本轮未改，测到的该阶段变化不作收益。

GUI真实15秒集成、干净制品、可逆替换、原生双模式/文件生命周期与最终推送回读尚待完成。
