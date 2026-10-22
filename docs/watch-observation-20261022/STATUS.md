# MacEvery 独立24小时观察

2026-10-22，r1，OBSERVING_NOT_FINAL。

固定App **0.1.3-g4dc3e4b**，源码 **4dc3e4b5d05136ecb75ca72eaa6dc3f6ce18b114**。独立窗口 **2026-10-22 13:22:00.017045 至 2026-10-23 13:22:00.017045**（Asia/Shanghai），86400秒，每60秒。没有重启、替换、重建App、发搜索或修改索引/配置；既有三进程继续运行。用户自然使用、模式切换和睡眠按实际身份分段。

已完成启动：私有根 `.build/watch-observation-20261022`；新state为其 `observation-24h`。唯一worker PID97567、cookie9393741510187、session `run-b55d8c6c568a4741a18f4a6fa66f4cf0`；实际arm64 Mach-O UUID、boot、executable、精确argv、写锁、committed prefix及三角色已核实。常量不能代替后续实际存活检查。首端点+0.052540958秒，此前无CPU覆盖，不补窗。精确哈希和普通使用起点见[启动证据](evidence/start.json)。

当前App/精确源码、验收基线和异常原件已冻结到本轮baseline；已有真实索引回滚按原路径/身份/既有SHA保留，没有再复制或全库hash。旧8096e0a/a4418a7/29b冻结版、旧67点与两个旧24h原件/逐小时材料/封存哈希保持；旧临时大库不重建。本轮不拼旧窗口。

每次先读本STATUS和[REPORT](REPORT.md)、新manifest/state/launch；比对已提交start.json与monitoring-tools中的4个private helper及2个public tool SHA。使用现成解释器 `/Library/Developer/CommandLineTools/Library/Frameworks/Python3.framework/Versions/3.9/Resources/Python.app/Contents/MacOS/Python`，PYTHONPYCACHEPREFIX仅本轮pycache。只运行本轮 `health_check.py`、`analyze_committed.py`（各自绝对路径且无参数），以及现有 `tools/observe_app_resources.py summarize --state` 指向本轮绝对state。**analyzer的任意--output不能防止覆盖其他私有原件，本轮只批准无参数默认派生缓存命令**；该限制已独立review记录，冻结helper不改。

health检查实际PID/cookie/UUID/boot/exe/exact argv/写锁和新鲜度，读committed prefix前核实实际journal字节数>=state.journal_bytes、连续sequence/行数/最后记录与state一致；若state推进，可重新读取同一snapshot验证一次，不修补原始证据。不能只信state.running或summarize。每个小时slot最多一组有界诊断快照，不能补旧slot。

关注UserDropped/必要恢复和watch单核>=50%有效分钟区间是否复发，关联有界日志的具体trigger/recovery计划/完成、revision与last_indexed_at，不能把batch OR flags当每事件原始flags，也不凭日志缺失声称无drop。分别保留所有process-measured高CPU候选和source/DB匹配的严格归因集合，不以DBunknown静默丢掉高CPU。GUI/serve/watch逐实际身份计算单小时CPU/RSS/footprint中位/p95/峰/端点、斜率和尾60平台；不跨重启/模式切换连线。

仅有显著复发、连续数小时增长候选、未解释长缺样/提前退出/身份失配或需用户操作时通知；正常及无需操作的已解释变化保持安静。锁屏无需暂停；睡眠/电源UUID变化/缺口独立记录，不补足/延长/重开窗口。禁止另启collector、sample插点、resume、搜索/status/control请求、全表COUNT、App/DB/配置/权限/工具修改、构建或压测。

到期：先确认window_finished、last_exit.reason=window_elapsed/exit_code=0、原worker实际退出或身份不存在且writer锁空闲、原始文件稳定；exit0本身不足以证明自然到期。保留manifest/state/journal/hourly/启动回执/原始有界诊断快照及逐文件SHA，保存脱敏逐小时摘要、完整异常时间线与实际wall/CPU覆盖/睡眠缺口/角色身份段。更新本REPORT/STATUS和本周登记，按既有授权提交推送收尾记录并git ls-remote精确回读远端SHA；App仍固定4dc，不随docs HEAD重建。完成或明确需人工处理后暂停同ID `macevery-48`，保留证据，不归档、不自动新开观察。窗口结束不是连续24h稳定/无泄漏PASS。
