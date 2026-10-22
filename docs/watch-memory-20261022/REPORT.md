# MacEvery 监听恢复与搜索内存定位记录

2026-10-22，r4，受测源码已推送，clean App **0.1.3-g4dc3e4b** 已替换并完成原生验收。源码 `4dc3e4b5d05136ecb75ca72eaa6dc3f6ce18b114`；基线29b6140保留，日常开发继续0.1.3，未分配正式发布版本。

本轮减少排队恢复放大和排除目录噪声，并限制长期积压计划。**尚未找到历史UserDropped及47.578125MiB footprint台阶的唯一底因，也不宣称整体内存下降。** [上轮24小时原始结论](../watch-observation-20261021/REPORT.md)、冻结版与真实索引回滚保持；本轮独立测试不拼入旧窗口，不自动开始新观察。

## 原因证据与改动

FSEvents回调与扫描本来就在不同线程，不能称扫描直接阻塞runloop。旧无界channel每次最多取4096事件后开始扫描，已经入队的多个恢复标记可能被切成多轮全根恢复。新的inbox在扫描前原子交换全部已收工作，只合并这一边界之前的恢复；扫描开始后到达的新丢事件仍要求恢复。UserDropped、KernelDropped、ID回绕和根变化保留全根语义，单独MustScanSubDirs仍仅恢复相交子树。

inbox限制16384路径和4MiB保留路径容量；worker跨失败批次的路径、重试和延迟清理同样预算化，超界压缩为必须完成的全根恢复义务。只有新鲜、无警告的全根扫描及清理完成才清除此义务并推进checkpoint。该预算不包含瞬时交换/折叠、扫描器summary及告警，不能当作整个监听进程4MiB内存上限。

原先只在callback内部过滤排除目录，系统仍会投递这些事件。现在最多8个已排除、存在、canonical准确且不含任何监听根的目录前移到FSEvents排除API；失败退回原过滤。上层组件匹配不再为每条事件建立Vec。保留排除目录改名、删除重建后的正确性；最终源码函数微测两块A/B/A，每次120万判断/70万命中全相同；240万次临时alloc/realloc及累计384MB请求量→0，CPU两A约0.242–0.260秒、候选0.102–0.119秒。该数值是匹配循环累计临时分配，不是节省384MB常驻内存。没有通过忽略恢复标记、跳过indexed_at代次写入或改变搜索候选/预算换速度。未改索引arena、排序或查询算法。

历史同进程/status在用户额外明确授权后只读取得，令牌未输出/保存：最后成功load/refreshed仍为10-21 14:33:12，owned index容量1,500,516,569B、ready=true/stale=true；计数单调不重置。因此可排除18:48成功全量/增量刷新及常驻arena增长。分配器/线程或短暂请求保留、VM页状态/记账的具体分摊仍UNKNOWN。只读vmmap被系统权限拒绝；最初凭据读取被自动审批拒绝，额外授权后才执行，未改权限绕过。历史系统日志无保留条目不证明未发生缓冲问题。

封存后、源码修改前10-22 10:44:20.110至10:49:59.313另一次flags3全根恢复访问/写入7,374,433条、elapsed339.210秒；独立原件留存，不并入24小时。历史四次恢复的callback到达时间缺失，不能据合成实验说四次都多余。

serve增加有界容量/fragmentation/刷新身份缓存、自身VM resident/internal/compressed/reusable/footprint、CPU与请求/查询原子计数。60秒timer不取搜索锁或DB，最多16待写、每秒2次刷新捕获、512KiB×2轮转、0600及链接检查；日志不含查询或原始路径。记录捕获时刻与缓存时刻分列，限频/轮转可遗漏，线程启动或I/O失败是best-effort诊断，不影响索引发布，不能称完整分配审计。搜索热路径只做原子计数，不增加查询缓存。`MACEVERY_SERVE_MEMORY_LOG=0`用于独立开销对照。

## 正确性与资源证据

[隔离数值及门禁](evidence/isolated-verification.json)保留逐查询/配对数据及全部回退项；原始结果、输入/制品哈希、失败和独立review在本轮ignored私有目录。

- Rust debug/release各191单元+8集成、Clippy、fmt、Swift检查通过。覆盖组合flags、排除/根外/重叠根、ID回绕、边界后事件、队列超界、失败重试、部分扫描/延迟清理、事务rollback及checkpoint。诊断快速load/fallback捕获也经修正和回归。
- 30k fixture与固定真实快照各6视图×24查询/更新case，CLI回退/Low Memory/Fastest完整有序六字段Top300一致，共288组；0超时，真实最慢4.501秒。service额外56响应体/13完整有序场景覆盖增量、事件溢出、journal缺口、外部写入、失败/恢复及并发限制。
- 真实FSEvents同树A/B/A六场景全部通过：排除洪峰后的正常更新、排除→包含改名子树、反向清理、删除重建、普通突发及后续改名，最终203行一致。进程CPU0.113158/0.092492/0.114210秒，候选比两侧A均值低18.64%；仅此小树混合操作，不是真实OS UserDropped故障注入。可见日志received由数千降为217，但限频计数不是系统总事件量；footprint是操作前后点，不是峰。

| 同物理母库A/B/A，6003记录合成积压 | 基线两A | 候选 |
| --- | ---: | ---: |
| 扫描前已收9000事件/3丢失标记：根扫描次数 | 3 / 3 | 1 |
| 扫描写入量 | 18009 / 18009 | 6003 |
| 扫描CPU | 317.634 / 312.596ms | 104.051ms |
| 入口CPU（HashMap合并成本增加） | 3.599 / 3.618ms | 9.788ms |
| 进程HWM RSS | 18.672 / 18.594MiB | 17.000MiB |
| 处理阶段采样峰footprint | 11.172 / 11.094MiB | 9.641MiB |

后到丢事件两版均保留两次恢复；注入失败不越过旧checkpoint，重试完成后才前进。合成确定时序不是历史事件重放。代次/13字段、ID及清理校验通过；减少的是冗余扫描次数，单次全根仍写新indexed_at代次，不能把revision改低伪装写放大消失。

真实快照packed短词两块A/B/A、29词×3次×6进程，522完整有序数组一致，最长297.028ms。code CPU普通中位+1.602%、配对中位+1.735%；整组+0.356%/+0.198%，通过既定+5%门槛。逐词两种中位均在5%内，保留`a`首块CPU+9.060%、wall+8.995%及nohit首块wall+5.931%的波动。常规9项packed另做两块A/B/A；完整结果一致，整组CPU+0.037%/配对-0.210%，plain(code) +0.845%/+1.192%，load CPU-0.412%、wall+0.015%。首个常规运行误用了legacy MemoryIndex，原件保留，不把其约4GB HWM当作App证据。

**内存没有整体下降结论。** 短词A/B/A索引容量全部528个点均1,500,165,798B；after_load footprint中位1109.799→1112.017MiB，code drop1542.541→1544.502，尾端/离散最大1542.603→1544.924（+2.320MiB，+0.1504%）。这是每进程88个离散点，不是连续峰。真实六view单次Fastest峰footprint另为1542.158→1543.439MiB（+1.281），RSS HWM1546.063→1547.328；常规packed HWM约1546.063→1546.071MiB。容量持平不能抹掉RSS/footprint的小幅增加，也不据此归因为日志。

候选日志off/on/off每组65秒，无查询/刷新，容量均固定。CPU分别0.000673/0.000747/0.000544秒；末footprint约1109.939/1109.955/1109.908MiB，开启比两侧中位高32KiB。只证明本次单轮空闲低开销，不证明长期无增长。小库机制实验59阶段packed容量/used/dead全部一致、177个SDK内存点有效；90次已记录全13字段/score有序Top300对照一致。候选lifetime CPU +2.073%、HWM +0.362%；独立操作计时+2.187ms/+13.05%，不是内存优化收益证明。释放DB/index后live malloc约99KiB，而allocator allocated为56/60/56MiB、footprint85.266/85.688/85.297MiB。支持对象释放后仍有allocator/OS保留机制，不能倒推历史47.578MiB原因，未使用purge/pressure/强制回收。

## 失败、边界与后续门禁

并行测试fixture文件名碰撞已增加进程内原子序号；初始Clippy/独立driver编译失败、权限/loopback sandbox失败均保留。机制controller参数曾容许第四批无恢复标记的错误范围，现收紧至8195..12288，默认实测9000不变，原文/hash保留。内存外部sampler四个临退出零footprint点保留并独立排除；不拿它们声称内存释放。一次临时显示脚本null百分比失败不影响原始benchmark。新工具修正均不改已运行原件或冻结production source。

## App替换、原生验收与最终回读

[安装及原生证据](evidence/native-acceptance.json)保留签名/payload、回滚校验及分角色资源值。源码4dc3e4b提交后精确回读远端main同SHA、fetch后behind=0并证明包含性；从clean20文件一致源码构建，签名deep/strict通过。原冻结CLI SHA为4f47823…，App签名CLI为0f8f5df9…。

首次去签名整文件比较失败原件保留：两副本799664B仅1字节不同，属于只读terminal `__LINKEDIT.vmsize`，32768→49152。签名实际从6416增到19840B，使signed LINKEDIT跨16KiB页；去签名仍保留旧vmsize。14个file-backed section及其余全部字节一致。独立复核后，仅在私有副本规范化这个已证明的8字节字段，且逐个校验raw签名范围/页取整、strip后无签名命令、readonly/no-sections LINKEDIT和其它每个字节；保留raw受测hash及严格签名门禁，没有重写App payload或泛化忽略segment。未做superblob内部槽完整审计。

CUA正常Quit后实查三进程全停、主库和全部sidecar无开放FD。只读SQLite backup到本轮新rollback，独立关闭副本quick_check及全文件SHA通过；普通RO验证副本的失败保留，只在已关闭/无sidecar/哈希固定的该私有副本上采用immutable验证，真实DB从未immutable。保留旧App原inode后独占重命名App-only替换；真实DB inode、database_id、roots/excludes保持，revision不回退，不迁移/整理/采用测试库。旧8096e0a、a4418a7、29b、两轮观察与既有回滚均保留；当前clean候选App和精确源码archive另留。

生产Swift七场景门禁通过：两次真实15秒请求分别15.001890/15.007102秒超时后启动CLI，300条完整六字段/顺序一致；取消子进程SIGTERM/wait4回收、不回填旧结果，冷请求确早于加载至少1.411秒；HTTP200真实no-hit与错误空结果分开，所有隔离子进程/端口清理通过。首次传短SHA被参数门禁拒绝，原日志保留，之后以完整源码SHA执行。

原生窗口Fastest/code300、Low Memory/code300（实际子进程argv=sqlite）及恢复Fastest/code300通过。真实文件新增/修改/改名/删除，窗口结果1/1/1/0；只读核对路径、kind、size、mtime/ctime、dev/inode及DB身份，旧路径和末文件均清理，未直接SQL写files。原查询、Fastest与Command-Shift-Space偏好恢复。新三角色PID/start/UUID/boot/exact argv与制品核对通过；模式切换的不同serve身份不跨段连线。

权限栏旧App显示Limited，新App在Check后为Unknown；相同Swift启发式在三个候选目录均不可见时返回Unknown，不能由此证明系统授权改变或保持。未改TCC/Full Disk Access，roots/excludes保持；受保护目录权限/内容不在本轮验收范围。当前配置范围的搜索与文件更新通过，不能扩展为全盘无权限遗漏PASS。

## 普通使用点（独立于受控A/B/A）

两次均No Query/Fastest、各约20秒，同一点内三个角色身份稳定。前为长寿命/旧加载代次，后经历重启、模式切换、原生查询及自然文件更新，索引内容代次不同；RSS/footprint点差不作为本轮因果收益。

| 角色 | 单核CPU，前→后 | RSS MiB，前→后 | footprint MiB，前→后 |
| --- | ---: | ---: | ---: |
| GUI | 0.000000% → 0.000000% | 115.000 → 143.188 | 52.845 → 51.985 |
| watch | 0.018498% → 0.004931% | 21.297 → 8.219 | 13.969 → 3.219 |
| serve | 0.000896% → 0.000982% | 6.063 → 1553.953 | 1549.158 → 1550.127 |

0只表示该短区间未测得CPU增量。serve footprint仍增加约0.969MiB；旧低RSS不表示owned index释放，fresh加载后的RSS回升也不能当作同输入回退。第一次安装后点与回滚SHA读取部分重叠，原样保留；上表使用之后无本任务负载/哈希重叠的独立quiet点，没有覆盖或拼接。

新原生serve有14条有限诊断快照，VM/CPU API均有效、与实际同身份匹配。首次加载后footprint约1117.830MiB，首批查询后约1549.9MiB；后续fixture增量capacity只增72B，最后三个idle点在120秒内footprint均1550.126648MiB，cached snapshot时刻与实际VM采样时刻分列。受控短词两版也都有首次查询后约432MiB台阶、容量不变，故直接支持“容量不变时出现与查询关联的footprint台阶”，VM/allocator具体分摊仍未知；不证明历史47.578MiB台阶原因或不必要长期保留。上游排除实际requested8/accepted=true。这里只做有限验收回读，没有启动新24小时采样器，短平台不作无泄漏或不再复发结论。

历史普通安装footprint+17.690%、上轮serve+40.844MiB/+2.717%、47.578MiB台阶及旧GUI台阶继续保留。UserDropped最初缓冲压力来源、历史重复恢复能否逐次避免、系统VM/allocator精确归因和长期趋势仍UNKNOWN。当前实现验证了可合并积压恢复、限制长期计划和减少排除噪声；真正后到/丢失恢复仍保留。收尾文档按授权推送并精确回读远端SHA，当前App保持4dc3e4b，不随docs HEAD重建；已有heartbeat继续暂停，不自动新开观察。
