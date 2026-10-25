# MacEvery e687扫描优化版独立24小时观察

2026-10-25，r1，PREPARED_NOT_STARTED。

固定App0.1.3-ge6878ef/source e6878efa2515e00ff35192eb29f4c6eb73a37b4b，不随docs HEAD重建/替换/重启。仅scanner/精确seen与固定根批次进入App，catch-up未进入；event_limit保守full路径保留，history_gap沿b027原路径。隔离与替换前/后原生验收、真实独立回滚、签名及源码精确远端回读闭合，见[交付](../scanner-only-20261025/REPORT.md)。

私有根`.build/watch-observation-20261025`，四个private helpers已从旧版复制，仅SOURCE变更，两个public tools字节不变。新baseline保留当前App和精确源码，旧数据/异常/全部冻结App/source及所有真实索引回滚原位保留；组合候选1b21及65秒VM调查保留，不拼接旧窗口。未创建state/worker，macevery-48仍PAUSED。启动只允许本轮launch_observer.py一次，无sample/resume/补足/延长/重开。

下一步：确认收尾记录精确推送和所有验收/资源子进程结束后，绝对路径现成Python启动一次86400秒/60秒唯一有界采样器；记录真实起止、worker PID/cookie/boot/UUID/exact argv/session及锁。每次heartbeat先读manifest/state/launch/start及工具SHA，再顺序无参数health、无参数analyzer、public summarize。committed prefix连续/行数/末记录/实际字节与state必须闭合；state推进仅重读一次，不修复原始。

重点UserDropped、watch单核>=50%分钟CPU、history_gap/event_limit实际重载与GUI/serve/watch同身份FP是否持续增长。所有process-measured高候选保留，另算无屏障且source/DB匹配严格集合；不得以DBunknown隐藏高CPU。原始admitted_raw逐flag与batch OR/寿命累计分开；idle缓存phase/method不重复当新full。身份/模式/睡眠/clock/gap分段，不跨段连线；每小时JSONL统计与尾60保留有效CPU时长，非正内存单列无效。单台阶或平台不证明/排除泄漏。

日志每小时slot最多active+一轮转，watch各256KiB、serve各512KiB，总25组/37.5MiB；startup另1024B一次。无源writer锁，无补槽；截断/轮转竞态、at_ms与snapshot_at_ms和身份计数连续性保留，缺日志不证明无drop/分配事件。正常无变化安静，仅显著复发、持续增长、未解释缺样/提前退出/身份失配或需操作时通知。

窗口开始后禁止App search/status/control、全表COUNT、App/DB/配置/权限/冻结工具修改、构建/压力测试和第二资源采样器。用户自然使用不干预。到期先闭合window_finished、window_elapsed+exit0、原worker退出/锁空闲/原始稳定，signal exit0不当到期。封存原manifest/state/journal/hourly/启动回执/日志与逐SHA；保存脱敏逐小时/完整异常/覆盖缺口/身份段并提交推送精确回读，暂停同ID，保留证据聊天，不自动新窗。
