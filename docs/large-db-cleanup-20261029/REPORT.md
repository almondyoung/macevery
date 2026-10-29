# MacEvery 历史大库清理与主干核对

2026-10-29，r1，COMPLETED_17_EXPIRED_DBS_REMOVED_LATEST_ROLLBACK_RETAINED。

用户明确允许清理此前保留的18份大库，并要求有效改动进入主干。先验证源码和真实运行库边界，再删除其中17份过期主库，保留最新一份实际安装回滚。此前“全部母本/历史回滚继续原位保留”的策略，从本次授权起对本表17个路径到期；旧清理、SHA和保留回执仍按原字节保存，不回写历史事实。

| 结果 | 数值 |
| --- | ---: |
| 已删历史主库 | 17份 / 83.924728 GiB |
| .build du 清理前→事后复核时 | 93.902706→9.979839 GiB |
| 目录净减少 | 83.922867 GiB |
| 同期卷可用空间净增 | 60.087803 GiB |
| 保留文件复核 | 31353份 |
| 保留小型WAL/SHM | 14份 / 493088 B |

du/路径尺寸与物理释放不同；APFS clone共享extent、写时复制和同期系统写入影响卷可用净变化，不能宣称独立物理释放83.9GiB。未清理系统快照、运行库或无关目录。

代码核对结果是无需新增源码合并。刷新origin后，13个历史源码提交（含完整组合候选1b21）均为本地main及origin/main祖先；29b/4dc/b027/e687四份冻结清单共84项source SHA可由对应Git提交完整重现。当前业务源码相对e687无差异，只有单一main和worktree，没有待合并的源码分支。原先未推送的2061f32仅为10月28日清理文档，可随本次收尾推送。见[源码审计](evidence/source-audit.json)。

catch-up并非遗漏：1b21候选已在Git历史，e687按用户选择撤回该路径。私有withheld-catchup.patch恰等于1b21→e687的db/server撤回补丁；此前短尾footprint约+218MiB的负向结论保持，不因清理重新启用。扫描优化已在主干和当前App，不需要重建、重启或替换。

最近真实回滚仍在`.build/scanner-only-20261025/installation/rollback/index.sqlite`，5118906368B；本轮重新完整SHA为`971e88895e300539072977743a8ee53f49b7561bdc2c016fad5a6717821e0ea9`，与原backup.local.json完全相同。它与同目录b027旧App配套，原closed quick_check=ok、无sidecar的历史验收保留；本轮没有执行SQL、恢复测试或新quick_check。当前e687 App和真实运行库保持。

17份待删主库全部重新完整SHA，临删核对device/inode/size/mtime/mode/owner/links，18库inode均区别于真实运行库，nlink均1且无保留文件共享inode。删除前两次精确lsof无打开FD或遍历警告；这仍是时点检查，不能宣称消除了全部TOCTOU。执行仅逐文件unlink，没有目录递归删除、SQL/checkpoint/VACUUM或源日志修改。每库有过期意向tombstone，另有逐库deleted回执和最终completed回执确认17个已删；意向本身不冒充已完成。

事后31328个保留文件（含最新5.119GB回滚）完整SHA核对，其余25个较大合成测试状态库仅按精确文件身份/元数据核对。当前App/冻结载荷/源码/helper/原始journal/封存/异常及原始SHA清单均在完整SHA范围，真实运行库device/inode保持。新Python固定解释器且禁止写pycache，未改变冻结观察工具、配置或权限，未发App search/status/control或新资源采样，未开启/拼接任何观察。

删掉的母本、历史回滚和两次native失败现场主库不再具备逐字重放能力。14份小WAL/SHM、copy/backup回执、测量、日志与全部异常时间线保留，尤其native-attempt2的非空WAL保留；这些小证据不能还原已删主库，也不能解决原生事故未知净变更范围。数据库不是源码，不合入Git；源码、补丁和小型冻结App继续保存。原观察部分窗口封存、历史OSdrop和内存负向结论不受覆盖。

## 已到期删除路径

| .build相对路径 | GiB | 删除前完整SHA256 |
| --- | ---: | --- |
| performance-round4-20261018/live-index-before-install-20261019.sqlite | 5.101215 | `bb92a5386d7080ddd6b7e88b55503256555454ecc9b096ede1b1f49703158ede` |
| performance-round4-20261018/live-index-before.sqlite | 5.100945 | `c2f97353d23ee90aab1117df5b00700416fe52019c1c6bbea74903aa8bc6c968` |
| performance-round5-20261019/live-index-before-install.sqlite | 5.106365 | `3fb99b611f3a4217efb45b7ed3788c7b3c1bfa8bf17c1bf8e83a25e2a2014cb7` |
| watch-memory-20261022/installation-r2/rollback/index.sqlite | 4.747414 | `63ae5b9997cedf35fc6aa7df03a745fbed7dbf8b51a8b69c59993a4688b4ed34` |
| watch-recovery-20261020/installation/rollback/index.sqlite | 4.688744 | `7cb000cd4ebe725c1b23be1f93a9deddb79161c0c2898259e8c90a0499c80f50` |
| performance-round3-20261018/live-index-before.sqlite | 5.100620 | `5a4f6ba128e381d3e31f0c894b922cfded0533c86f4cb71b4445f9fd33738997` |
| performance-round2-20261017/live-index-before.sqlite | 5.096542 | `c18594824e5bda777213be4324ff0617b2006f5b26fa04711afd7806e314a3af` |
| performance-round6-20261019/live-index-before-install.sqlite | 5.111450 | `dd4db6f36f1b059d71a57ad10752f399de722fd92721fa19e301d892a6c2ced8` |
| performance-round7-20261019/live-index-before-install.sqlite | 5.113625 | `8e4b2d3b2cd09052f67be1ab69684cf4390a4342614492ba028712a2cc1194e8` |
| scanner-only-20261025/isolated-raw/catchup-memory-tail/mother.sqlite | 4.766369 | `24f6965b216af024a2f95383601e9c1ed725ca6243bf8aeffb19bfc5b41183f8` |
| scanner-only-20261025/isolated-raw/native-attempt1/index.sqlite | 4.766365 | `fea5bbe6b132ea68a51d93e3488d0dba5150eb87b91a22f02e41ab68cebeed51` |
| scanner-only-20261025/isolated-raw/native-attempt2/index.sqlite | 4.766365 | `3c68916fc8c443f6f7ab12b512dfbaa3c0ca2b786fa94912388bfbef45779a1f` |
| performance-round1-20261017/snapshot.sqlite | 5.039104 | `68bd22081af1eb5ec9164037407cb6c9c13349a7f8e460fa9aa682b3286b5b29` |
| recovery-refresh-20261023/installation/rollback/index.sqlite | 4.766365 | `ba6f642423e9b16737dfe09c055b9d1a398d7ec761dd27bf1db413a14d7f3c50` |
| recovery-batch-20261025/isolated-raw/frozen-input/index.sqlite | 4.766365 | `ba6f642423e9b16737dfe09c055b9d1a398d7ec761dd27bf1db413a14d7f3c50` |
| recovery-batch-20261025/isolated-raw/validation/catchup-aba/mother.sqlite | 4.766369 | `24f6965b216af024a2f95383601e9c1ed725ca6243bf8aeffb19bfc5b41183f8` |
| topk-consistency-20261019/install-state/live-backup.sqlite | 5.120506 | `9ad4610dc0926cbeeb2b6d47b2087b65df04976b3b52e4aa3da2b33548c670fd` |

逐文件执行证据在ignored `.build/large-db-cleanup-20261029`，脱敏清单见[清理摘要](evidence/cleanup.json)。本次代码无需新增回归测试；执行前后身份/SHA、Git包含性、文档链接和diff检查构成此轮验证。数据清理已完成；文档提交/远端精确SHA结果由私有publication回执及聊天收尾报告确认，不能仅据此状态文件推断已推送。不会随文档HEAD重建App。
