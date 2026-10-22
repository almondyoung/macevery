# MacEvery修复版独立24小时观察

2026-10-22，r5，Asia/Shanghai，SEALED_WITH_WATCH_CPU_RECURRENCE_AND_UNRESOLVED_FOOTPRINT_STEP。Owner：当前聊天。

## 固定版本与结果

- 固定App0.1.3-g29b6140，源码 `29b6140575cdd69c965732b943dccda77e8b586e`。本轮没有构建、重启或替换App，没有改配置、系统权限或观察工具。观察/诊断流程未主动写入真实索引，正常监听更新继续；结束时原三角色仍原位运行，payload哈希与冻结manifest一致，真实DB文件身份保持。
- 窗口2026-10-21 10:18:28.343991至2026-10-22 10:18:28.343991，原单一worker自然结束：window_finished、window_elapsed、exit0，实际原PID已不存在、writer锁空闲。1441点/1440有效CPU区间，每角色86363.194709秒；首36.825831秒无差分，终端连续时钟跨截止20.543ms（墙钟192.969ms）按原规则保留，不补齐、不续开。
- App/DB1441点match，三角色均known且各单一身份；session/boot/power各一个，只有initial_endpoint barrier，无>1ms睡眠段或已标记的缺样/身份断点，分钟间峰仍可能漏测。
- 监听24段单核CPU≥50%、峰82.080153%，捕获四轮flags=3的roots恢复，总indexed/rows_written29,472,762；16:35后无新增≥50%段。UserDropped底因、全根恢复必要性/可合并性仍UNKNOWN，不标长稳PASS。
- GUI/serve/watch末footprint52.860/1549.142/13.985MiB，末几小时未呈持续上行；GUI尾60点+96KiB和serve18:48大台阶后再+48KiB保留。serve47.578125MiB台阶根因、不必要保留/泄漏均未证实，低RSS不代表索引释放或footprint恢复。旧普通安装点和旧窗口异常不覆盖。

## 可追溯入口

- [最终报告](REPORT.md)、[数值摘要](evidence/final.json)、[逐小时CSV](evidence/hourly.csv)、[完整异常线/覆盖边界](evidence/anomaly-timeline.json)。小时CPU按右端点分桶，slot24仅终端单点，不是第25小时。
- 原始/冻结资料及SHA副本：`.build/watch-observation-20261021/sealed-20261022`，298文件25,431,857字节，复制前后/原件SHA均核对；manifest SHA `da48dab10b1f8e1337b136a46c3c9434689918db04cc33b23536e75713e4c529`，journal SHA `07f5e9c65cdd6d33a275454b49e9cf5405c276119bc409391b8f524e74162bd3`。
- 最终分析与两个独立复核、r4旧文档、精确远端SHA、自动化暂停回执保存在 `.build/watch-observation-20261021/final-20261022`；观察派生统计失败及修正保留，最终已从原始前缀重算。
- 原小时快照缺槽6/8/13/19/22，资源端点不缺；44原始日志文件含两次异常副本均通过单文件完整性门禁，限频/事件采样/轮转/非原子成对快照仍不能给完整触发审计。所有旧冻结版/真实回滚/67点和旧24h保持，过期大测试库未重建。

## 收尾边界

观察与封存已完成，结果按用户授权提交推送并精确回读远端；本次同ID `macevery-48`在收尾推送后暂停，回执随final目录留存。不自动新开观察、不归档聊天。当前App保持29b，不随文档提交重建。

搜索/逐文件索引内容本窗口未验，不把控制元数据和扫描完成写成新增正确性PASS；上一轮验收与失败保持。后续若继续优化，应另建独立任务/基线，优先定位UserDropped及重复全根恢复，再用有界分项证据调查serve台阶；本次不实施源码或App变更。
