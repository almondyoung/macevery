# MacEvery 第二轮内存与重载优化

日期：2026-10-17；版本：r1，源码与隔离验证完成，本机 App 安装验收待补。

本轮以第一轮提交 `ce375fe` 为基线，压缩常驻索引，减少无效全量重载及重载时的双份内存。用户已授权替换本机 App、提交和推送；保留 0.1.3，使用开发构建 SHA，不创建发布 tag。

## 变更

- 内存记录由完整 String/FileRecord 改为 104 字节记录、分块字符串 arena 和扩展名字典；路径/小写路径/文件名复用已有后缀，保留全部结果字段及稳定排序。memory/compact 偏好均使用 packed 存储，原偏好名称和默认 memory 保留。
- SQLite trigger 在文件记录实际变化的同一事务中递增 revision，普通 meta、roots、excludes、checkpoint 与 no-op UPDATE 不递增。包括 indexed_at 的所有结果字段都参与判断。外部 SQLite 写入也会触发。
- 服务保留只读 observer，通过 revision、数据库 ID 和设备/inode 识别提交及数据库替换。加载时的 revision 和全部记录来自同一 WAL 读快照；竞争请求只执行一次 lazy reload。
- 重载先释放旧索引，再加载新索引。失败清空可用状态并返回 503，后续可重试；临时缺失不新建空库，旧格式库替换会安全迁移。SQLite 模式复用一个 Mutex 保护的只读连接。
- status 仅观察，不触发加载；显示实际索引分配容量、freshness 和重载次数。GUI 的 Index 为容量而非 RSS。构建写入 GitCommit、BuildTime 和开发 BuildLabel，支持单独输出候选 App。

## 同一真实索引 A/B/A

冻结数据与第一轮相同：7,226,486 条，SHA256 `68bd22081af1eb5ec9164037407cb6c9c13349a7f8e460fa9aa682b3286b5b29`。每个独立进程使用自己的 APFS 克隆；两轮串行 A/B/A，4 个基线进程、2 个候选进程，9 类查询，limit=300。driver 将 baseline 的完整记录内存后端与 candidate 的 packed 后端比较，后者即新 memory 服务内部存储。完整、有序的结果逐字段编码后哈希全部一致。

以下为各组中位数，CPU 是完成固定查询负载消耗的处理器时间。

| 指标 | 第一轮基线 | 第二轮 | 变化 |
| --- | ---: | ---: | ---: |
| 加载后 RSS | 3.71 GiB | 1.82 GiB | 降低 50.8% |
| 搜索进程 RSS 峰值 | 3.72 GiB | 2.64 GiB | 降低 29.0% |
| 9 类查询累计 CPU | 6.194 秒 | 6.350 秒 | 增加 2.5% |
| code 查询 CPU | 0.464 秒 | 0.392 秒 | 降低 15.5% |
| 加载 CPU | 6.773 秒 | 6.342 秒 | 降低 6.4% |

压缩解引用有代价：部分 glob、无命中、过滤查询更慢，整组查询 CPU 未下降。首组基线查询为 5.695 秒，后续基线约 6.19–6.31 秒；候选 6.292/6.409 秒，保留全部样本与配对结果，不能称所有查询都加速。加载后 RSS 也不是搜索后常驻值，候选首次搜索可升至约 2.64 GiB；不能用 1.82 GiB 代表全过程。

## 真实服务重载测试

同一冻结数据增加两条固定探针，A→B 顺序启动独立、带认证的 loopback memory 服务，执行相同元数据变化、外部 INSERT/UPDATE/DELETE、搜索与显式 reload。此为单次机制验证及暖缓存观测，不替代 A/B/A 延迟统计。

| 指标 | 第一轮基线 | 第二轮 |
| --- | ---: | ---: |
| 元数据变化后搜索 CPU | 5.640 秒 | 0.039 秒 |
| 元数据变化后搜索耗时 | 19.051 秒 | 0.039 秒 |
| 含多次重载的服务生命周期 RSS 峰值 | 11.95 GiB | 2.64 GiB |
| 同一生命周期累计 CPU | 28.691 秒 | 9.991 秒 |

候选元数据变化及 status 查询均未 reload；外部文件增删改后 status 只标 stale，下一次搜索恰好重载一次并返回正确字段，显式 reload 再增加一次。基线的状态观察还引发额外重载，累计内存受分配器保留页影响；此峰值不能与固定查询场景混为一个数。实际 packed 分配容量约 2.33 GiB，不等于 RSS。

CPU 使用 getrusage/wait4 和进程级 libproc；Apple Silicon 的 libproc CPU Mach ticks 经本机 125/3 timebase 换算，并与 getrusage 自检。阶段 RSS 20 ms 采样可能漏峰，生命周期峰值使用 wait4；不以宽松的单次预算门槛证明无泄漏。内核页缓存未清空，其它用户后台任务保留。

## 正确性与回归

- Rust Debug/Release 各 75 项通过，严格 Clippy all-targets 通过；包括完整字段独立 DB 解码对照、i64 极值/NULL/空串/Unicode、分块边界与稳定同键排序。
- 覆盖 WAL 提交/回滚、元数据与 no-op 不刷新、并发仅一次加载、一致快照、数据库替换/旧库迁移、路径暂缺和失败恢复。
- Swift GUI model/真实子进程检查通过，包括老新 status 解码、取消、过期结果、503/429 背压和 CLI 回收。此项本身不等于原生窗口验收。
- 30 万条合成索引的真实 FSEvents A/B/A：9 类创建/修改/重命名/删除/排除与后续恢复检查通过，最终状态一致。watch CPU 为基线 0.0161/0.0165 秒、候选 0.0175 秒，属毫秒级差异，不宣称本轮进一步降低 watcher CPU。
- 40 请求真实 HTTP 验证通过：2 个正确 200、38 个 503，认证、控制接口、后续搜索恢复和进程/端口回收通过。

## 安装与证据

原 App 和真实索引已分别备份在 `.build/MacEvery.rollback-20261017-before-round2.app` 与 `.build/performance-round2-20261017/live-index-before.sqlite`（7,292,429 条）。不重建用户索引，不更改 roots/excludes/性能偏好/快捷键。原生窗口及真实索引增量更新验证将在安装后补录；远端 CI 和正式 QA 独立记录。

结构化证据：[搜索](evidence/search-summary.json)、[服务重载](evidence/service-summary.json)、[watcher](evidence/watch-results.json)、[并发](evidence/burst-summary.json)、[源码与制品指纹](evidence/candidate-artifacts.json)。完整原始结果、响应、RSS 样本和受限环境失败保存在忽略目录 `.build/performance-round2-20261017/`，包含本机路径的索引与完整结果不推送。

复跑入口为 `tools/compare_round1.py`、`tools/check_service_round2.py`、`tools/watch_round1.py` 和 `tools/check_service_round1.py`，完整参数保存在原始 manifest/report。默认沙箱阻止本地监听，授权后隔离服务验证通过；一次自动审批超时后，纯查询基准在现有沙箱权限内完成。Python 系统 cache 写入受限后改用任务临时 cache，通过语法检查，均属环境证据。

仍保留的边界：文件变化后的首次搜索会等待全量 reload；health 不等 reload 锁，status 会等待进行中的 reload。HTTP cancel 不终止已开始的 Rust 查询。自动模式仍使用保守估计选择后端，SQLite 既有候选限制未改。无界 watcher channel、逐行 SQL prepare/upsert 及更细粒度索引增量替换留待后续。
