# MacEvery 第一轮优化状态

更新：2026-10-17，Asia/Shanghai。

## 当前结论（最终）

第一轮本地实现与验证完成。开发标识 `0.1.3-ga7e6d8a-dirty`，没有替换用户当前 App / 索引，没有提交、推送或发布。

- 723 万条冻结真实索引、两轮 A/B/A：9 类查询完整结果和排序一致；累计 CPU 中位数 22.556 → 5.443 秒（降低 75.9%）；进程 RSS 峰值 9.60 → 3.72 GiB（降低 61.3%）。加载后 RSS 仍约 3.71 GiB。
- 30 万条受控 FSEvents A/B/A：9 类生命周期检查及最终索引状态一致，CPU 3.173 → 0.017 秒（降低 99.5%，只代表该场景）。
- Rust Debug / Release 均 55/55，strict all-targets clippy、make test-gui、格式及文档链接检查通过。真实 HTTP 40 请求背压、控制请求可用、恢复及临时进程回收通过；远端 CI 未运行。
- [最终报告](REPORT.md)、[测量比较](evidence/comparison.json)、[制品与源码指纹](evidence/artifact-manifest.json)。原始数据与冻结索引保存在 `.build/performance-round1-20261017/`，不随报告外发。

后续仅在用户要求时替换当前应用，或开展第二轮常驻索引压缩 / 增量 reload；原生窗口全链路和实际后台负载安装前后观察尚未执行。以下保留实施检查点，勿重复开展已完成工作。

## 目标与范围

在保持各后端原搜索结果、排序与索引更新正确性的前提下，优化 watcher 提前排除和路径清理、查询预处理、Top-K、过期查询治理。保持 memory 默认后端，不改常驻索引结构；不替换当前运行应用、不写真实索引、不推送、不发布、不自动递增版本。

## 基线

- 源码：main / a7e6d8a5b5f6cd58008db11ce2d6fc6a4638d1a8，开始时工作区干净。
- 冻结源码：/private/tmp/macevery-round1-20261017/baseline-src。
- 当前应用、serve、watch 保持原进程；实际索引约 723 万条。前轮只读观测 serve RSS 3.71 GiB，watch RSS 375 MiB、10 秒 CPU 17.1%，不是受控 A/B。
- 优化前后使用同一冻结索引和合成夹具；结果差分、CPU 时间、RSS 与高水位分开记录。

## 当前进展

- 已分三条独立修改线：search/compact；watch/scanner/db；GUI 查询调度及 CLI 回收。
- 主线程负责 server 并发、隔离基准、集成验证与报告。
- 首次 fetch 在沙箱因 SSH 网络受限失败；只读升级重试成功。当前 main 领先 origin/main 5 个既有提交、behind=0，基线不属于远端主干；本轮仅本地验证，不发布。

## 验收

1. 各后端优化前后查询结果逐字段一致，覆盖 Unicode/glob/fuzzy/filters/ties/limits。
2. 创建、修改、重命名、删除、目录替换、排除、临时失败恢复无丢失或误删。
3. 过期请求不回填、不启动 fallback；大 stdout/stderr、取消、超时能回收。
4. 相同后端/数据/负载对比查询 CPU、峰值内存和空闲 RSS；不得将合成实验当作真实桌面验收。

## 原计划收尾（已完成）

实现、定向差分测试、Swift 编译、顺序 A/B/A、独立复核和性能报告均已完成，以上方最终结论为准。

## 实施检查点

- 只读 SQLite online backup 完成：7,226,486 条，5,410,697,216 字节，15.3 秒；副本 /private/tmp/macevery-round1-20261017/snapshot.sqlite。
- 查询差分：1,230 组完整结果与 10,865 次评分对照通过；GUI 生产编译与受控回归通过。
- 第一次集成 Rust：52 项中 51 通过；prefix_cleanup_uses_path_index 的临时文件清理出现 NotFound（查询计划已正确使用路径索引），交由负责者修复，原失败保留。
- 独立复核要求：永久权限警告只重试失败子树；失败退避不得延迟新正常事件。修复后再冻结候选并运行最终基准。
- Python py_compile 首次受系统缓存目录沙箱限制，改 PYTHONPYCACHEPREFIX 到任务临时目录后通过；属于环境边界。

## Final candidate checkpoint

- Rust 55/55 and strict all-targets clippy passed; same-process scan generations remain monotonic through a clock rollback.
- GUI bounded busy retry expanded to 3.45s total; the fake service recovers after 1.05s, stale requests still cannot retry/fallback.
- Initial updated GUI test retained an old request-count assertion and failed; the assertion was corrected and full verification passed.
- Final Rust performance suites now run sequentially, with two real-index A/B/A blocks. No live app replacement.
