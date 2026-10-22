# MacEvery 监听恢复与搜索内存定位

2026-10-22，r3，ISOLATED_GATES_PASSED_NATIVE_PENDING。Owner：当前聊天。

目标：保留29b观察/冻结/回滚，定位UserDropped与serve footprint；针对性优化，通过完整搜索/更新与CPU/内存门禁后替换App、提交推送。日常版本保持0.1.3，开发标识用精确clean源码后缀。

已完成：有界扫描前inbox及worker恢复义务、最多8个保守上游排除、无逐事件Vec分配；serve有界VM/容量/traffic日志。历史底因UNKNOWN范围见[REPORT](REPORT.md)，不宣称整体内存下降或历史每次恢复冗余。旧24h/67点、8096e0a/a4418a7/29b App及回滚均保留，没有新开观察。

最终production freeze `.build/watch-memory-20261022/candidate-frozen`：CLI SHA256 4f47823d47bde409f5cb8016b5464ddf5842c53f2c95083aa8375b7141f32a4a，rlib 5cab7f2df1d27a2851ed084a0ff5d5bc851f9c6eed31925218bb855bf4721a28，20源文件逐hash一致。源起点9052bd1，基线production29b6140，App仍原位29b三进程。

已通过：Rust debug/release191+8，Swift、Clippy/fmt；两库288组完整有序Top300、service13结果场景；真实FSEvents六场景A/B/A；确定性恢复12 runs；真实29短词和9常规packed两块A/B/A、CPU+5%门禁；小库13字段内存机制；诊断off/on/off65秒。原始/失败与独立复核均在本轮ignored私有根，[公开隔离证据](evidence/isolated-verification.json)。短词离散footprint+2.320MiB事实保留，容量相同；没有整体内存减少PASS。

接续顺序（不得跳过）：
1. 最后独立review/工具边界核对，形成精确clean source commit；验证与frozen20文件一致。
2. 真实15秒超时/取消/冷启动完整Top300；推送源码后fetch/behind=0/远端包含性及ls-remote精确SHA。
3. 从该clean源码构建独立candidate App；核对签名/normalized tested CLI/hash、配置、权限和实际角色身份。
4. 旧App普通资源点；CUA正常Quit、实际全停/DB FD空闲；本轮installation工具只读独立备份真实索引并校验，保留原App原inode后App-only替换。真实DB不adopt/整理/改SQL。
5. CUA启动与原生Fastest/LowMemory code300、真实文件增改名删、恢复原偏好/查询；新角色身份/日志/最终普通资源点，所有门禁闭合后提交推送验收文档/登记本周、精确回读远端SHA。

若锁屏影响原生验收，请求解锁后继续；不能跳过或改系统权限。已有macevery-48仍暂停；不自动开启新24h。当前没有替换、重启、改真实DB数据或推送本轮源码。
