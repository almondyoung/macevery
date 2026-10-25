# MacEvery e687扫描优化版独立观察

2026-10-25，r2，独立窗口已一次启动，后续覆盖待实际采样记录。App0.1.3-ge6878ef，source e6878efa2515e00ff35192eb29f4c6eb73a37b4b。只采用扫描优化，catch-up未进入App。旧329文件封存、b027后续封存、所有历史窗口/67点/冻结App/source/真实回滚均保留，不拼接。

本轮不是历史优化因果A/B。扫描小树CPU约−33–36%、每批等待约+0.82秒，短词code+0.16%、逐项最差+3.11%在+5%门槛内；未复现OS UserDropped底因。旧组合候选刷新CPU−94%但短尾FP+218MiB、延至65秒仍约+221MiB且full reusable237.7MiB/catch-up0的负向事实保持；该候选未安装。新窗首端点只作普通使用基线。

历史事实保持：b027窗4批drop根恢复、11个高分钟峰72.208859%，代次未变写已跳过但遍历CPU仍在；2次event_limit可见、history_gap无可见完成，不证明日志缺失/退出缺样部分0次；原角色凌晨退出，用户回忆可能系统升级而未确证重启。4dc窗1轮drop/6高分钟峰76.03%、7390140代次写放大与19:25 history_gap；29b4轮/24高分钟峰82.08%与serve+47.578MiB；更早9分钟峰85.6349%无原始触发。普通安装/TopK/短词FP上涨及GUI历史台阶未知，不被新窗覆盖或当整体内存下降。

本轮启动前原生验收含临时根20秒不收敛和Quit后UI自动重开候选连接真实DB的事故，均独立保留于[交付与事故记录](../scanner-only-20261025/REPORT.md)。这些不是新观察数据，未拼入新journal。禁止在启动后继续隔离内存试验或主动App请求。

本轮按实际身份和屏障统计UserDropped/高CPU/重载、三角色单小时中位/p95/峰/端点/斜率/尾60及CPU有效覆盖，原始与日志不完整边界如实保留，到期统一封存。

独立窗口2026-10-25T14:56:01.038606+08:00至2026-10-26T14:56:01.038606+08:00（Asia/Shanghai），86400秒/60秒；唯一worker PID4436/cookie15485059573653/sessionrun-80c67ea0ccb24a6198f3877b9c9c60b6已实查，arm64 UUID匹配实际slice、exact argv/boot/锁闭合。首端点+0.056920250秒此前无CPU覆盖，不补窗。

首端点[source/DB pin match、三角色known](evidence/start.json)，health实查原worker alive且writer锁被持有，原始prefix字节/连续sequence/末记录与state一致。只建立普通基线，不能从一个端点断言UserDropped、高CPU未复发或FP平台。到期统一统计实际wall/有效CPU、未覆盖项和全部异常。

启动交接已核对早期committed prefix与同ID自动跟进ACTIVE，见[交接](evidence/handoff.json)。仅几分钟有效CPU，尚不判断全窗复发、长期增长或平台；日志限频/截断/快照时段边界继续保留。
