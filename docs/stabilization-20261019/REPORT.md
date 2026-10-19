# MacEvery 固定版本与收尾验证记录

2026-10-19，r2；本机限定验证。固定版与原证据保留；用户后续授权先修TopK再重新观察24小时，原48小时窗口已封存且未完成，非正式发布或公司QA。

## 固定版本

固定 `0.1.3-g8096e0a`，源码 `8096e0a9c61cacd3402cd3017e95fc60ddb2c0f3`，原验收文档提交 `d7b6073`。没有增加版本号、改写tag或替换App。源码 archive、独立App副本、签名和二进制哈希已保存，见[冻结清单](evidence/frozen-version.json)。原始副本位于忽略目录 `.build/stabilization-20261019-8096e0a/`，保留第七轮回滚与历史失败证据。

GUI SHA256 `265b3ed6c370cb04c653beb5613d1f19e3ebf8b2298ebe52be77b9365306eece`；已签名CLI SHA256 `422e776c583ff9769c613c5f915f1b5c4e993da4085fa434042b81bd82a710ee`。本轮只新增验证/观察工具及文档，不修改搜索、索引、GUI生产源码或系统权限。

## 权限提示

第七轮记录的 Limited→Unknown 保留，未将其解释成已确认授权丢失。本轮开始检查时App已退出；从同一路径重新打开固定版本后显示 Limited。

旧版 `2f32302` 与固定版的 `detectPermissionStatus` 实现相同。它异步执行，以 `fileExists` 过滤三个探测目录，全部不可见时显示 Unknown；列目录失败只汇总 Limited，不保留错误。初始化状态也是 Unknown，不能区分尚未完成、不存在和不可访问。

在实际GUI进程21972中临时LLDB附加，只读取HOME、bundle身份及三个探测目录的stat/list结果，随后detach。UID501；Foundation、FileManager与HOME一致；Mail/Messages stat成功、list失败，错误为 NSCocoaErrorDomain 257，底层POSIX 1（EPERM）；Photos探测可读。因此当前 Limited 有直接证据。没有列出目录内容、修改权限或读取TCC库，详见[脱敏探测回执](evidence/permission-probes.json)。独立helper的权限结果没有当作GUI结果。

历史 Unknown 缺少当时错误码，具体原因 **UNCONFIRMED**。当前probe可读或失败也不能证明所有索引根/受保护目录的覆盖。以后若修改提示，应另行区分 Checking、Missing、Denied和探测失败，并保留结构化错误；不为收尾诊断重编译固定App。

## 实际短睡眠与原生恢复

自动唤醒的非交互管理员认证不可用（sudo要求密码）；用户确认手动唤醒。本任务未设置、取消或覆盖电源计划，也未关闭其它应用的电源断言。

第一次黑屏约1秒后亮屏，Mach睡眠时钟未推进，保留为**未实际入睡**。第二次日志：16:19:39进入Software Sleep，16:19:41由网络事件触发DarkWake，16:19:48转FullWake。用户等待时间足够，但系统提前唤醒，不能称为持续1分钟睡眠或长睡眠验收。Mach连续/awake差约0.393秒作为独立辅助证据，不能用屏幕黑屏时间替代真实睡眠。系统自身重排了wake计划，前后文本不一致，本任务没有执行排程写入。

该次睡眠前后GUI/serve/watch仍为21972/21975/21976，boot、进程启动cookie/UUID、索引database_id和roots哈希一致。实际FSEvents与原生窗口完成：新建16字节、修改36字节、重命名后仅新名存在、删除后0结果；只读SQL核对临时条目清理为0；`code`返回300条，清空和快速替换的最终窗口状态正常。原生端点观察不代替延迟回调竞态验收，后者由下述模型集成另验。见[系统及原生回执](evidence/sleep-wake.json)。

这些是端点和功能证据，不跨睡眠计算CPU，不把端点RSS下降当内存优化或无泄漏证明。

## 启动时立即输入

冻结CLI使用独立临时库、随机认证loopback端口。后端四组fresh服务顺序运行：就绪后查询参考、加载中保持code、关闭旧连接、快速替换连接。所有实际200响应的六字段和顺序都与同一冻结后端的就绪oracle一致；源码/输入库未变，进程组、端口、HTTP线程和临时库均已清理。见[后端摘要](evidence/early-http.json)。

| 后端场景 | ready耗时 | 场景累计CPU | 场景末RSS |
| --- | ---: | ---: | ---: |
| 就绪后查询参考 | 15.127秒 | 5.070秒 | 1.485GiB |
| 加载中保持code | 14.782秒 | 5.083秒 | 1.485GiB |
| 关闭旧连接后查询 | 14.716秒 | 5.103秒 | 1.485GiB |
| 快速替换连接 | 13.767秒 | 5.296秒 | 1.485GiB |

早期发送至加载完成回执约13.754–14.956秒；保持code的完整响应为14.956秒，clear/rapid旧连接在约54–55毫秒即关闭。rapid最新连接实际经历两次明确503后重试200，没有把无响应写成503。这里不证明GUI的15秒超时，后述模型集成单独验证。

这不是同等负载性能A/B：各场景工作量不同，参考的额外oracle请求在场景边界后执行，退出CPU仍如实包含。关闭连接不证明服务立即停止计算，0.5秒尾观测也不证明所有弃用工作已完全排空。这里不验证GUI取消发布或原生首帧。

Swift生产模型与冻结已签名CLI、7,226,486条隔离索引的集成七项通过，见[真实模型验收](evidence/gui-startup.json)：启动立即code（HTTP路由，14.269秒）、inflight清空、快速替换仅最新结果、warm HTTP code，以及两次真实URLSession 15秒超时后的CLI成功/取消、最终清空。clear/rapid的stale publications均0；超时分别15.067/15.063秒，成功CLI CPU0.027718秒、HWM约31.45MiB，取消child为-15并有wait4；所有oracle/测试服务、harness和wrapper自有进程组/端口清理通过，输入未变。

HTTP/CLI分别按实际路由校验完整六字段与顺序，warm明确禁止CLI回退；实际200没有伴随传输错误。计时外独立memory oracle生成后已回收，再启动被测服务，首个查询早于loaded_at的整秒边界。oracle会预热页缓存，该套为功能集成，14.269秒不能当冷盘启动基准、原生首帧或新的性能A/B。15秒超时由暂停工具自己创建的服务触发，没有暂停运行App或模拟URLSession超时。

### 保留的跨后端限制

首次大库Swift验收错误地用CLI oracle校验Fastest HTTP，因而失败，原证据 `gui-real-1` 保留且清理成功。进一步核对：CLI的SQLite路径在limit300时最多取24000候选后排序，Fastest从全量索引取TopK；`code`两组300结果仅9条路径交集，共同记录六字段一致。该候选上限在前一版已存在，不能称为本轮回归，也不能把两条路径说成全局等价。

修正测试后分别固定HTTP与CLI oracle，并按实际成功路由验证通过；这只修复测试的错误假设，不消除生产中回退结果可能变化的限制。[第一轮报告](../performance-round1-20261017/REPORT.md)已保留各backend候选策略及SQLite候选截断；本轮补入README，保持冻结边界，**不宣称Fastest与CLI回退Top300等价**。统一TopK语义需要单独评估低内存路径的CPU、内存和超时预算。

## 48小时资源观察

原窗口计划为10月19日16:39:14至10月21日16:39:14（Asia/Shanghai）。后续用户明确授权先修复CLI/Low Memory/Fastest TopK一致性，验证和替换后重新开始24小时。原collector在核对启动身份后受控停止，67个样本和全部state保留，heartbeat已暂停，记为STOPPED_BY_NEW_SCOPE；窗口没有完成，不拼入新24小时，也不称长稳通过。历史开始时采用每60秒只读采样，不发搜索、不COUNT全表、不改库、不自动重启App，不阻止系统睡眠；[开始回执](evidence/observation-start.json)仅代表当时初始端点。

为清除临时LLDB诊断驻留影响，在窗口开始数分钟后正常退出并重新打开同一个固定App；保留原始记录，按新启动身份分段，前一诊断进程段不用于长期内存趋势结论。新进程的启动/受控原生验收及最初预热期也需与后续普通使用分开，不能把这次受控重启当作自发崩溃或泄漏恢复。

历史独立collector为PID35412，启动cookie/boot身份保存在本机state，现已退出；heartbeat `macevery-48` 封存时已PAUSED，后复用为Top300修复版独立24小时ACTIVE；旧collector及state仍封存。观察未完成，后续以[TopK修复任务](../topk-consistency-20261019/STATUS.md)为准。

干净重启后新GUI/serve/watch为36166/36172/36173。临时文件新建19字节可搜到，删除后窗口0结果且只读SQL无残留，见[固定App最终回执](evidence/final-app.json)；没有再次附加调试器。查询清空后的约20秒[点采样](evidence/final-point.json)：GUI/serve/watch平均CPU分别0.05815%/0.00082%/0.02395%（单核口径），末RSS分别约142.7MiB/1.535GiB/19.4MiB，总计约1.693GiB、0.08292%单核。保留其它应用与用户后台工作，不能与前轮不同工作量样本组成受控A/B，也不能据此提前声称长稳或无泄漏；本轮没有生产优化改动。

CPU只连接身份一致且没有睡眠/电源变化、采样缺口、时钟异常或collector重启的相邻端点；unknown不填0，PID重用按启动cookie/boot/UUID分段。小时摘要保留实际覆盖，采样峰值可能漏掉短峰和短生命周期CLI；不能把进程HWM相加称为同时峰值。48小时到期只代表观察窗口结束，需复核覆盖、趋势和异常后才能下结论。

`tools/observe_app_resources.py` 提供init/sample/run/summarize，单worker锁、固定绝对截止、JSONL追加和原子state；已提交日志被截断时拒绝回退，partial尾保留并拒绝自动续写。25项mock和临时自有SQLite边界测试通过，不代表48小时实际运行已通过。汇总见[检查回执](evidence/checks.json)。

复核时先读本目录STATUS，再运行 `python3 tools/observe_app_resources.py summarize --state <STATUS中固定state目录>`；summarize只读取已提交证据，不查询进程或数据库。需要检查collector仍存活时，必须核对state中的PID与启动cookie，不能只凭PID或上次写入的running字段。不能因记录中断而自动重启App、改权限、重新固定身份或静默补写缺失小时。

## 证据边界与后续

原始索引、完整搜索结果、目录路径、token和进程环境不提交；这里只保存脱敏数值、哈希和错误代码。正常使用窗口与受控测试分开，原始失败不覆盖。远端CI、正式QA、持续1分钟以上睡眠、全部受保护目录访问和跨后端TopK统一均未验证或未实现。
