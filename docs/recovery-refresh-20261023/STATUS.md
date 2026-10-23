# MacEvery 全根恢复写放大与搜索刷新优化

2026-10-23，r3，ISOLATED_ACCEPTANCE_COMPLETE_GUI_AND_INSTALL_PENDING。用户已授权实现、隔离正确性/性能验收、验证通过后替换App及提交推送；未授权自动新开24小时观察。

当前main起点2c89f5e353d5c75dab5a2b559247f487765e16fe；运行App仍0.1.3-g4dc3e4b/source4dc3e4b5d05136ecb75ca72eaa6dc3f6ce18b114。旧24小时自然结束，原采样器退出、同ID自动跟进暂停。旧329文件封存、原异常/基线/冻结App/source/真实索引回滚保持，不修改、不拼窗、不再次清理旧材料。

本轮范围：scanner/db的访问代次与内容写入合同；server的history_gap/增量恢复；GUI无用解码/回调工作与有界诊断；UserDropped因果证据缺口。新私有根`.build/recovery-refresh-20261023`。先使用隔离fixture/封闭快照，不用运行App作开发负载；最终App-only替换前保存可恢复副本并闭合验收。

门禁：完整有序Top300、搜索结果全部字段、增删改名/外部SQL/重复ID/取消/15秒超时、恢复/重试/stale清理及失败事务；code等短词CPU/延迟不退化（沿用明确+5% CPU门槛并保留局部回退）；同输入CPU/RSS/FP A/B/A与加载/写量。若涉及indexed_at或revision语义，必须明确合同并维护跨backend一致，不能静默跳增量/发布部分同步。未运行的验收不能写PASS；平台/单点不证明无泄漏。

已实现：刷新专用内容no-op和精确有界seen（scanner24/DB53/watch原42定向通过）；history_gap完整快照差异（server25/compact27通过）；GUI解码前取消/weak可取消重试（Swift隔离回归通过）。watch逐flag计数/匿名寿命统计/独立startup摘要实现，定向测试进行中。见DESIGN.md明确indexed_at只对refresh修订，index_paths/generic仍原合同。所有修改尚未提交；运行App/真实DB/配置/权限未动。

本轮基线：既有4dc冻结CLI SHA0f8f5df95b2275ee29a2fdec7314e7752723da645ced096f25131ca589117284；精确archive解出baseline/source并release lib构建exit0，rlib SHA270bd2f2259e3dd7de9759462c6c74982a58c0bf37bf8927f7dbf8029b1dd4db。新owned tmp `/private/tmp/macevery-recovery-refresh-20261023-son1i91q`，30000行对抗fixture、5.097GB原回滚的APFS新clone/immutable quick_check ok；未重hash或SQL打开原回滚/真实库。

新增tools/perf_recovery_round10.py和check_refresh_round10.py，分别覆盖生产恢复路径A/B/A及真实HTTP强制history_gap的同输入A/B/A；控制器已静态复核/修正code300、真实15秒wall门禁、失败证据/owned清理，py_compile通过。未开跑计时负载。后续原TopK helper用显式--baseline-sha256传4dc，不用旧默认值。

下一步：
1. 明确正确性合同与最小实现，记录设计/未归因；冻结本轮基线。
2. 实现并做窄正确性/完整回归，再同输入资源对比；保留失败与负对照。
3. 通过门禁后提交源码、构建干净开发制品、可逆App-only替换及原生双模式/code/增改名删验收。
4. 文档/本周登记/证据、提交推送和精确远端SHA回读；不自动新开观察。

2026-10-23阶段更新：Rust debug/release210+8、Clippy、18次恢复A/B/A、两个Top300矩阵、大库history_gap R2及短词A/B/A完成；具体数字/失败与FP取舍见REPORT r1/evidence/isolated.json。初始2秒diff回退保留，最终预算6秒；目前GUI资源A/B/A进行中，真实15秒集成需要提交源SHA后运行。仍未替换App/推送，不自动新观察。
