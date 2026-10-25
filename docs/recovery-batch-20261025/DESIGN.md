# 实现合同

2026-10-25，r1，实施前合同；不是验证通过或安装报告。

- scanner只对所有现有内容字段完全相同的行走prepared SELECT快路，保留实际ID与原indexed_at；变化/新行走原upsert并重新读取写后ID，generic upsert不变，失败和seen预算仍回滚/legacy。
- 根恢复在现有1秒绝对批次deadline内合并，普通局部更新仍按150ms idle flush。deadline不重置；take边界后事件仍进入下一轮，checkpoint不得越过未完成义务。不声称能消除所有OS UserDropped或第二批恢复。
- journal catch-up仅Packed后端event_limit使用：最多16384事件（retention）、8192唯一ID、32MiB文本、1秒合作式预算；普通4096快路保持。固定同一读快照target，验证完整连续journal和old/new/conflict ID最终状态，完整read commit后一次apply/stamp发布。history_gap仍独立snapshot reconcile；预算/非单调ID/碎片/失败保留原安全full或invalidate。
- 合作式时间检查不能称硬端到端15秒；索引锁等待、SQLite busy及后续apply仍需要真实HTTP完成时限验收。较大delta与保留旧arena可能增加临时/尾部FP，必须保留资源取舍。
- 原App、库、配置、权限、冻结观察helpers和旧证据不改；只允许新/tmp owned fixtures/services。先正确性后同输入资源对比，不自动开启新观察。
