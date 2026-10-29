# NSTAR simulator — Rust Web backend

Axum/Tokio + SQLx SQLite。单 crate，使用同目录 `static/`，无需前端构建。
原 Python Web/CLI/GUI 实现已删除；Rust 重写的计划与验收记录归档在 [docs/history/](docs/history/)。

## NSTAR-only 与旧库迁移

生产入口现在只提供 **NSTAR 现货与永续**：注册默认 NSTAR、公共行情/交易接口只允许 NSTAR，不启动 FXSIM、FX、股票或 Binance 行情任务，也不为这些品种按需请求外网。NSTAR 行情、撮合、强平、资金费和债务检查保留。

旧品种规格、历史成交与旧代码暂时保留。`Store::open_legacy` 仅供显式兼容测试/工具使用，生产命令没有多品种开关。容器部署本次修改需自行构建镜像；现有上游 `latest` 不包含本地未发布改动。

**含非 NSTAR 选中币种、持仓或挂单的旧库会拒绝启动，不会自动删除数据。** 先停止所有访问该数据库的服务，然后执行一次性迁移（以下为仓库根目录 Linux/Git Bash 示例）：

```sh
cargo build --release --locked
./target/release/fx-sim-rust \
  --db "$PWD/data/fx_sim.db" \
  --migrate-nstar-only \
  --migration-backup "$PWD/data/before-nstar-only.db"
```

Windows 二进制名为 `fx-sim-rust.exe`。迁移不需要管理员密钥或静态目录、不监听端口，完成后输出 JSON 摘要并退出。

- 备份目标必须是新文件，父目录须存在；不覆盖已有文件。SQLite `VACUUM INTO` 备份包含已提交 WAL 内容，不是简单复制主 `.db`。备份创建即限制权限：Unix 为 `0600`，Windows 使用仅当前用户与 SYSTEM 的受保护 ACL；Windows 目标卷必须支持 ACL，权限验证失败则拒绝写入备份内容。
- 先备份并同步备份文件，再在一个事务内删除所有非 NSTAR 永续仓位、挂单和现货资产条目，将选中币种改为 NSTAR。
- **直接删除，不按市价平仓、不结算浮盈浮亏、不退还现货成本，不改变账户现金余额、借款/债务或 NSTAR 仓位。** 保证金占用随被删仓位消失。删除的非 NSTAR 现货资产不会兑换成现金。
- 保留账户、密码/token、历史成交、充值记录、旧K线及旧品种定义；每个修改账户写入 `nstar_migration` 管理审计。受仓位/资产删除影响的短期收益样本清空，避免制造虚假的5分钟涨跌。
- 迁移失败会回滚账户与审计更新，保留备份；没有待迁移账户时返回零计数，不创建新备份。迁移期间必须保持停服，不能与其他写库进程并行。
- 恢复时停服并使用备份副本和兼容的旧版本，不能把备份直接交给 NSTAR-only 生产入口而跳过迁移检查。备份含账号凭据，按生产数据库同等权限保护。

## 内存热路径与持久化

- 风险检查从已提交内存账户计算，完整账户/选中币种均未变化且没有成交时不执行 SQL；同一轮真实变化的多个账户与成交在一笔事务中提交。各账户在 rayon 线程池上并行试算（默认使用全部 CPU 核心），但只接受仍基于当前价格和 ID 的结果：一旦某账户成交推动了 AMM，其后的账户按新价格重算。结果与逐个顺序结算完全一致（有等价性测试）。
- 行情缓存只在内存交换时短暂加写锁，风险批次提交（含 fsync）期间不阻塞任何报价读取；tick 在风险批次运行时释放 NSTAR 盘口，`/api/market` 与资金费率读取最近一次发布的面板。风险批次与 K 线刷新使用保留的写入通道，客户端突发下单不会让强平批次失败重试。
- 资金费与债务重置各自一轮只提交一个批次；下单滑块的最大可开数量在账户未变、价格变动 <0.2% 时最多复用 3 秒（下单时仍会完整校验）。
- 唯一后台写入者使用有界 admission/队列；入队后即使 HTTP 请求取消，也完成提交与缓存发布。交易成功等待提交完成，不采用未落库先确认。队列满或等待超时返回服务不可用，客户端应刷新账户/历史确认结果，不能盲目重发订单。
- 生产 SQLite 使用 WAL + `synchronous=FULL`；显式 `open_legacy` 工具仍为 NORMAL。FULL 依赖操作系统/存储正确实现 flush，并不等同于实测断电保证；本项目仍只支持单进程写同一数据库。
- K线按 `(symbol,周期,时间)` 在有界内存中合并完整 OHLC，每秒批量保存；失败保留待写版本重试，成功只清除已提交版本。过期清理只在窗口前进时执行。未刷新的K线在进程崩溃时可能丢失，积压超过容量会明确拒绝推进，不无限增长。
- NSTAR 已提交订单流量在取消/失败后保留，下一次交易定价前或 tick 重试恢复；正常停机先停止生产者、排空账户写入，再恢复待处理流量并刷K线。支持 Ctrl+C，Unix 也处理 SIGTERM。强制 kill 无法走正常排空。
- 排行榜后台每5秒生成共享快照，GET不写数据库。默认Top50，响应含 `total/as_of`，认证请求另含本人 `me`（带rank）；`offset/limit`按需分页，单页最多100。5分钟收益基线使用每账户181槽内存环形缓冲，**服务重启后需重新积累约5分钟**；旧 `equity_samples` 历史暂保留但生产不再逐轮写入。
- WebSocket 客户端可发送 `{"type":"view","visible":true,"leaderboard":false,"interval":"1m","low_power":false}`。后台标签暂停展示行情/榜单，低耗模式降频，账户认证和控制事件仍保留。行情/账户/榜单使用最新值槽，慢客户端不会累积无限展示帧；重连需要重新同步。
- 前端使用增量蜡烛、持仓线/盘口节点复用与可见性合并渲染；每个 tick 只写入文本、样式或状态真正变化的节点，各面板用 CSS containment 隔离重排。低耗设置在浏览器保存，开启后同时关闭所有过渡动画。图表库优先本地同版本资源，保留固定版本/SRI兜底。
- 静态白名单资源首次加载后驻留内存，支持 ETag/304；**更新静态文件需与服务一起重启**。静态 gzip/Brotli 可在反向代理配置；不默认压缩每个高频 WS 帧。
- 管理员可用已登录的管理 Bearer token 请求 `GET /api/admin/performance`，查看持久化队列/提交/无变化跳过/失败计数及待保存K线数量。计数不是物理磁盘IOPS；线上仍需结合系统磁盘指标。

## 独立启动

在仓库根目录运行（已验证 Rust 1.98.1）：

```sh
cargo build --locked
mkdir -p .pi/rust-dev
```

创建**独立测试**管理员配置。`admin_key` 与容器入口使用同一实现，不覆盖已有文件；未设置 `FX_ADMIN_PSK` 时随机生成口令并只打印一次：

```sh
FX_ADMIN_PSK='换成测试口令' cargo run --locked --example admin_key -- .pi/rust-dev/admin.json
```

启动：

```sh
./target/debug/fx-sim-rust \
  --listen 127.0.0.1:8000 \
  --db "$PWD/.pi/rust-dev/test.db" \
  --static-dir "$PWD/static" \
  --admin-key "$PWD/.pi/rust-dev/admin.json"
```

打开 `http://127.0.0.1:8000`，注册测试用户；管理员页面 `/admin`。Ctrl+C 关闭连接、后台任务与数据库。静态目录、数据库、管理员文件均为显式参数，支持任意工作目录；默认监听回环 8000。目录须预先存在。

同一数据库只能由一个服务进程写入。账户 token 存在浏览器 origin 下，换端口不会自动继承旧登录。

局域网访问时，启动参数加 `--listen 0.0.0.0:8000`，通过 `http://<本机局域网 IP>:8000` 访问；数据库、静态目录和管理员配置参数不变。HTTP 未加密，仅用于可信网络。

## 容器（all-in-one）

`ghcr.io/d3-3109/fx-game-simulator` 是含服务端、静态资源与 PSK 生成器的单镜像，支持 `linux/amd64` 与 `linux/arm64`。也可用本目录的 `docker-compose.yml`，或用 `docker build -t fx-sim .` 本地构建。

```sh
docker run -d --name fx-sim -p 8000:8000 -v "$PWD/data:/data" \
  -e FX_ADMIN_PSK='换成一个足够长的口令' \
  ghcr.io/d3-3109/fx-game-simulator:latest
```

与直接运行的区别：

- 数据目录固定为 `/data`，含 `fx_sim.db` 与 `admin-key.json`；**必须挂卷**，否则容器删除即丢账户与历史。
- 首次启动从 `FX_ADMIN_PSK` 生成管理员配置（PBKDF2 20 万轮，与 `App::admin_login` 同一实现）。未设置时随机生成并**只在启动日志打印一次**，用 `docker logs fx-sim` 查看。已有 `admin-key.json` 时不覆盖，改口令须先删该文件。
- 无参数时入口脚本自动拼出 `--listen 0.0.0.0:8000 --db /data/fx_sim.db --static-dir /app/static --admin-key /data/admin-key.json`；显式传参则原样覆盖。
- 容器以 `fxsim`（uid 10001）运行；以 root 启动时入口脚本先把 `/data` 交付给该用户，再降权 `exec`。
- NSTAR-only 生产运行不连接外部行情，也不需要 `FX_PROXY`；旧行情依赖及证书包暂时保留供兼容工具使用。
- 镜像自带 `HEALTHCHECK`，探测无需鉴权的 `/api/symbols`。
- 按 IP 限流（IPv6 按 /64）：接口每秒 200 次（突发 1000），登录每秒 5 次（突发 60），WebSocket 每 IP 最多 500 个；注册每 IP 每小时 20 个、每天 100 个，全站每小时 500 个。注册配额可用 `FX_REGISTER_PER_IP_HOUR` / `FX_REGISTER_PER_IP_DAY` / `FX_REGISTER_GLOBAL_HOUR` 调整（填 `0` 关闭该项），接口速率用 `FX_API_RATE`（每 IP 每秒次数，`0` 关闭）。放在 Caddy/nginx 后面时**必须**设置 `FX_TRUSTED_PROXY`（地址或网段，如 `172.16.0.0/12` 覆盖 Docker 默认网络），否则所有玩家共用代理那一份额度；此时不要再把 8000 端口暴露到公网。

CI 见 `.github/workflows/container.yml`：先跑 `test_chart.js`、`fmt`、`clippy -D warnings`、`cargo test`，通过后才构建推送；PR 只构建 `amd64` 不推送，`main` 与 `v*` 标签推送双架构，随后用真实镜像做一次管理员登录与注册冒烟。

## 验证

需要 Node（前端图表回归）。从仓库根目录执行：

```sh
node test_chart.js < static/app.js
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
cargo build --locked --example fixture_server
```

所有测试使用临时数据库/回环临时端口/测试管理员配置，不读取真实用户数据或 Tiingo key。`fixture_server` 是测试工具，不是生产入口。

保留的旧多品种外部行情 smoke test 仅用于兼容验证，**不属于 NSTAR-only 生产路径，且需单独执行**，不能用网络故障替代确定性回归结论：

```sh
cargo run --locked --example market_smoke
```

最新验收记录见 [ACCEPTANCE.md](ACCEPTANCE.md)。

## 兼容与边界

以下 FX、股票、Binance 等说明仅描述暂时保留的 `open_legacy` 兼容路径，生产入口不启用这些行情。

- NSTAR 使用多时间尺度行情：3–7 天周级方向、2–6 小时日内波段、5–36 分钟局部震荡/高波动/趋势，以及 1–3 分钟短线冲锋。周级方向会移动长期基准，不再被回归固定 100 抵消；周内仍有独立多空波段。短线冲锋同时移动公允价值和震荡中心，配合 Student-t(4) 厚尾噪声，满仓 10x 在强行情中可在 5 分钟内触及强平，但行情生成不读取玩家持仓，低杠杆面对同一走势仍有更高生存率。模型思路参考 [CKLS/OU 外汇生成器](https://github.com/kasna-cloud/dataflow-fsi-example/blob/main/app/python/src/forexgenerator/forexgenerator.py) 与 [多因子市场模拟](https://github.com/Jackson-Wozniak/Stock-Market-Simulation)，参数按本项目的游戏速度重新标定。
- NSTAR 现货与永续共用同一弹性 AMM 价格、流动性和订单流：偏离锚价 x（对数）处流动性为 `depth·(1+x²/w²)`（`depth`=50,000，`w`=0.08），锚价约 5 分钟半衰跟随价格。同一锚价下拆单与一次扫单同价、往返回到原价。现货全款买入、不会强平；永续使用保证金、资金费与强平。玩家自愿订单立即影响盘口，且 35% 冲击转化为公允价值；强平只产生临时执行冲击，不改写公允价值。
- 全站账户杠杆最高 10x，旧账号更高杠杆读取时自动降为 10。NSTAR 按合约毛持仓分段计保证金：≤1 万枚 10x、≤2.5 万 5x、≤5 万 2x、≤10 万 1x，超过 10 万的新增仓位仍 1x；现货与永续均**无总持仓硬上限**，以全额资金需求和 AMM 滑点约束巨型仓位。单笔委托保留数值安全边界，`/api/state` 的 `limits` 给出当前最大可开与档位。
- 委托：现货和永续共用限价、stop、stop-limit、部分成交与撤单撮合；永续另支持市价单附加止盈/止损。现货成交时才检查现金/持币，不足则失效（`void`），不另做资金冻结。每账户现货与永续合计最多 20 笔挂单。NSTAR 下单提交时先撮合被穿过的挂单再打印价格，挂单吸收的冲击不会在 K 线上留下影线。
- 强平标记价以公允价值为内部指数，最新价相对公允价的溢价限制在 ±5%（对数）并以约 60 秒半衰平滑，成交仍按实时盘口。强平仓位进入队列，按约 10 秒半衰释放且每 tick 最多 10 NSTAR，避免大资金强平形成无限市价瀑布。公允价值回归 100 的拉力随偏离平方增强，持续单边拉盘仍会被 软性区间 拉回；没有 12.5/800 硬价格上下限。
- **剩余风险：** 这是游戏经济而非无偿付能力证明。管理员充值、多账户和真实趋势仍可造成巨额盈亏；负余额目前进入既有债务/重置流程，尚未实现独立保险基金或 ADL。其他外部报价品种的建仓校验也未完整扣除所有即时点差亏损，不适用于真实资金交易。
- FX 实时报价、K 线只用 biquote.io，不再自动回退 Yahoo；CNH（离岸）禁止替代成 CNY（在岸）。共享后台每 500ms 批量拉取所需 FX 报价（约120次/分钟目标）；单轮串行执行，慢请求跳过错过的时隙、不积压或补发。周线由同源日线按 UTC 周一聚合。Binance 共享上游 WS 广播，FXSIM 本地 tick 聚合。
- FX 顶部/图表使用源 `mid`，BUY 显示/成交用 `ask`，SELL 用 `bid`；平多用 `bid`，平空用 `ask`，浮盈与强平同口径。FX 不再叠加模拟点差；加密货币、FXSIM、离线 fixture 保留原模拟点差。行情是 broker 的指示性报价，不保证锁价成交。
- 报价缓存按本机接收时间保留 8 秒，交易时效仍独立看源时间戳，避免源价不动时各页面重复拉价。429/503 遵循 `Retry-After`（缺失默认60秒），网络失败短退避；上游失败可保留旧报价供显示，但交易、自动风险检查拒绝超过 30 秒、过期标记或闭市的报价及缺少换算价。FX 时间使用源 `timestamp`，不把旧价重新标成新价；按钮报价过期显示 `—` 并禁用开仓。
- REST 遵循 `FX_PROXY` / HTTPS/HTTP 代理环境；Binance WS 与原 Python 一样直连 TLS。不要把含凭据的代理 URL 打入日志。
- `storage.rs` 独占 SQL 和完整业务事务。SQLite 单连接 + 全局写锁保证当前单进程模型的原子性；没有多实例写入或吞吐保证，也没有预建通用数据库框架。
- 保留旧 schema、账户 JSON、普通账号密码算法/token、USD/LEGACY 历史；旧 `trades` 缺 `currency` 时补列标为 LEGACY，不改旧金额。`fx_state.json` 属于旧单用户辅助路径，当前多用户 Web 使用 SQLite；Rust 不读取/覆盖它。
- 新生成的 position ID 唯一并可恢复，但不保证复制 Python 惰性载入造成的编号空洞；已保存 ID 原样保留。契约对照只对新 ID 做稳定引用映射，时间/token 等非确定值不逐字比较。
- 普通登录按旧行为轮换持久 token，旧登录 token 在本进程内暂存直至重启；管理员会话仅内存保留，重启需重新登录。
- 交易/审计写失败返回明确错误且回滚；非法 JSON、过大请求等安全拒绝，不复刻 Python 的意外崩溃。模拟随机数不要求和 Python 逐步相同，真实 OHLC 聚合/持久化相同。
- 静态资源只开放固定清单，保留旧 `/charts.js`、字体/图表许可证和 5.2.1 SRI/CDN 兜底。HTTP 本身不加密，仅供本机/可信网络模拟用途。
