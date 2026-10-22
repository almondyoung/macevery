# MacEvery 监听恢复与搜索内存定位

2026-10-22，r4，COMPLETE_WITH_UNRESOLVED_HISTORICAL_CAUSES。

当前App固定 **0.1.3-g4dc3e4b**，源码 **4dc3e4b5d05136ecb75ca72eaa6dc3f6ce18b114**。源码main已推送并精确回读同SHA；clean构建/签名/逐字节payload、App-only替换、原生验收及最终资源回读闭合。验收文档随本次收尾推送，远端精确回执保留私有目录，不在文档自引用最终SHA。不随docs HEAD重建App。

完成：有界inbox/worker恢复义务、扫描前边界合并、最多8个保守上游排除、无逐事件Vec分配；serve有界VM/容量/traffic日志。debug/release191+8、Swift/Clippy/fmt、两库288组Top300、service、真实FSEvents六场景、确定性恢复12 runs、短词/常规packed A/B/A、小库13字段机制、诊断off/on/off均通过。真实15秒超时/CLI回退/取消七场景与完整300条通过、隔离清理闭合。

原生：Fastest/code300、LowMemory实际sqlite/code300、恢复Fastest；真实文件增改名删1/1/1/0与元数据/旧路径清理通过。原查询/模式/shortcut恢复，实际三角色身份/制品/配置回读通过。权限UI为Unknown（此前Limited），启发式源码未变，未改TCC/FDA；受保护目录未验收，不能扩展成全盘完整性PASS。

资源：合成已到达3drop恢复3→1，扫描/代次写量18009→6003、扫描CPU约-67%；真实小树事件A/B/A进程CPU-18.64%。code CPU+1.602%、短词聚合+0.356%在既定+5%门槛内。短词离散FP尾端+2.320MiB、容量相同，普通安装serve FP+0.969MiB事实保留，不能宣称整体内存下降。完整口径和全部边界见[REPORT](REPORT.md)、[隔离证据](evidence/isolated-verification.json)、[原生/普通点证据](evidence/native-acceptance.json)。

保留：旧24h/67点原件、8096e0a/a4418a7/29b冻结App与源码、原真实回滚、本轮新真实索引回滚/旧App原inode/clean候选与源码、原失败和独立review均在.build相应目录。封存manifest SHA da48dab10b1f8e1337b136a46c3c9434689918db04cc33b23536e75713e4c529不变；过期历史大库未重建。

未闭合历史原因：UserDropped缓冲压力来源、历史每次恢复是否冗余、47.578MiB及旧GUI footprint台阶唯一归因、长期趋势仍UNKNOWN。此次是机制修复和诊断补齐，不作无泄漏或长期不复发PASS。既有macevery-48保持暂停；没有新采样器/续窗/拼窗或自动新24小时。
