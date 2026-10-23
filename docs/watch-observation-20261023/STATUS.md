# MacEvery b027新版独立24小时观察

2026-10-23，r1，OBSERVING_NOT_FINAL；同ID heartbeat已恢复ACTIVE并从实际配置精确回读。先读本STATUS/REPORT，不重读长聊天。

固定App0.1.3-gb027a42/source b027a42c2cb859235b2a354c4c4bd8187a785a77，不随docs HEAD构建、重启或替换。全套隔离/原生验收和App-only收尾90298444f57f400677a3fc8aa762b933faf24db8已推送并精确回读。原三角色仍原位；App/精确source/验收/普通点/全部旧数据异常冻结及真实索引回滚保留；新普通serve FP+5.453MiB/+0.349%不抹去。

本窗2026-10-23 15:24:53.957679 至2026-10-24 15:24:53.957679，Asia/Shanghai，独立86400秒、每60秒唯一采样。私有根/Users/yangtao/pine/code/macevery/.build/watch-observation-20261023，state其observation-24h。启动worker PID79741/cookie11644312669864/session run-d7ced1b2db9c4c4aa286c872ead14a37，只是比较值，未来须实查；实际arm64 UUID97fb6671edf933f680849f34e400b6ac已匹配Mach-O slice、boot/executable/精确argv与writer锁。首端点+0.057852083秒，此前CPU无覆盖，不补窗。3端点首复核通过，未sleep/gap/身份切换；尾部仅约2分钟，不评价平台或泄漏。

每次先读manifest.local.json/state.json/launch.local.json和已提交evidence/start.json，核验其四private/two public SHA与monitoring-tools.local.json及实际文件一致。只用/Library/Developer/CommandLineTools/Library/Frameworks/Python3.framework/Versions/3.9/Resources/Python.app/Contents/MacOS/Python，PYTHONPYCACHEPREFIX仅本轮pycache。按序绝对路径无参数health_check.py、analyze_committed.py，然后tools/observe_app_resources.py summarize --state本轮绝对目录。analyzer已移除任意state/output参数，只写latest-trends.local.json缓存；冻结后不改helpers/publictools。

必须验证实际JSONL字节>=state.journal_bytes、committed prefix行数/连续sequence/末记录==state，若state推进可重读同snapshot一次，不截断/修复/补写。实查worker PID/cookie/UUID/boot/exe/exactargv和writer锁/新鲜度/唯一session，App fingerprint/DB pin及三角色身份不能只信running。绝不另启collector/sample/resume/补足/延长/重开或拼旧窗。禁止向App发search/status/control、全表COUNT、改App/DB/配置/权限、构建压测；用户自然使用/模式切换/睡眠按实际身份/屏障分段，不干预。

重点UserDropped、watch单核>=50%分钟CPU、history_gap全量回退及三角色FP是否持续上行。所有process-measured高CPU候选均保留，另算source/DB匹配且无sleep/gap/unknown/clock/identity屏障严格集合；不因DBunknown隐藏高段。history_gap要读full_after/fallback_full真实reason，与snapshot_reconcile/idle缓存的旧方法分开；计数按实际身份/session/operation keys去重。只有idle缓存可列未闭合候选，不当新成功重载次数或零。原始逐flag/admitted_raw/寿命累计与batch OR分开，不能当独立OS drop或丢文件数。

首个有界日志槽内已捕获1次event_limit full_after/fallback_full（非history_gap）；属于保留的保守回退，FP同期回落而RSS略升，未唯一归因。保留诊断与resource实际时序，不默写“无全量加载”。首次可见子集无UserDropped标志/累计增长、无>=50%分钟；不等于整窗无复发。每小时slot最多一次四文件active+一轮转快照，watch各256KiB/serve各512KiB，最多25slot/37.5MiB；startup单独baseline1024B且configured非started。无App writer锁，FD/路径竞态、截断/尾行、限频/轮转丢失不补槽，快照需窗口与prefix截止/实际身份过滤。

按角色/每个boot-PID-cookie-UUID-exe身份逐小时CPU/RSS/FP中位/p95/峰/端点/斜率/尾60；不跨重启或模式切换连线。累计hourly summary非单小时；保留有效CPU时长、sleep/电源UUID/gap；小时CPU均匀分配仅估计；尾60未必60分钟，末单点不当第25小时；非正内存无效单列。平台/单台阶/RSS回落都不能证明无泄漏或FP恢复。历史全部未决详见REPORT，不以新窗做因果AB。

正常无变化安静；新UserDropped/高CPU/history_gap fallback或持续数小时增长候选、未解释长缺样/提前退出/身份失配才通知。自然到期须window_finished、last_exit.reason=window_elapsed且exit_code0、原worker实际退出、writer锁空闲、原件稳定；signal exit0非到期证明。封存完整raw/启动/诊断/逐小时/异常时间线和逐文件SHA，汇总实际wall/CPU覆盖与sleep/gap/未覆盖项，不能称连续24h稳定/无泄漏PASS；更新文档/本周登记并提交推送+ls-remote精确回读。完成或明确需人工后暂停同ID macevery-48，保留证据、不归档、不自动新窗。
