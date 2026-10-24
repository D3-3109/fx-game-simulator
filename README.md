# FX simulator — Rust Web backend

Axum/Tokio + SQLx SQLite。单 crate，使用同目录 `static/`，无需前端构建。
原 Python Web/CLI/GUI 实现已在切换后删除（见 git 历史与 [ACCEPTANCE.md](ACCEPTANCE.md)）。

## 独立启动

在仓库根目录运行（已验证 Rust 1.98.1）：

```sh
cargo build --locked
mkdir -p .pi/rust-dev
```

创建**独立测试**管理员配置；此命令交互读取密码，不写明文、不覆盖已有文件：

```sh
python3 - <<'PY'
import getpass, hashlib, json, os, secrets
path = '.pi/rust-dev/admin.json'
password = getpass.getpass('Test admin PSK: ')
if not 1 <= len(password) <= 256:
    raise SystemExit('PSK must contain 1–256 characters')
salt = secrets.token_bytes(16)
config = {'salt': salt.hex(), 'hash': hashlib.pbkdf2_hmac('sha256', password.encode(), salt, 200000).hex()}
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w') as f:
    json.dump(config, f)
PY

./target/debug/fx-sim-rust \
  --listen 127.0.0.1:8000 \
  --db "$PWD/.pi/rust-dev/test.db" \
  --static-dir "$PWD/static" \
  --admin-key "$PWD/.pi/rust-dev/admin.json"
```

打开 `http://127.0.0.1:8000`，注册测试用户；管理员页面 `/admin`。Ctrl+C 关闭连接、后台任务与数据库。静态目录、数据库、管理员文件均为显式参数，支持任意工作目录；默认监听回环 8000。目录须预先存在。

生产数据库 `fx_sim.db` 与管理员配置 `.pi/admin-key.json` 已由 Rust 服务直接使用；不要同时再启动旧 Python 进程写同一库。账户 token 存在浏览器 origin 下，换端口不会自动继承旧登录。

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
- 镜像内 `ca-certificates` 供 Binance WebSocket 校验 TLS（该连接走系统信任库、不读代理变量）；FX REST 走 `FX_PROXY`。容器内的 `127.0.0.1` 是容器自身，代理应写宿主机名，例如 `FX_PROXY=http://host.docker.internal:7890`。
- 镜像自带 `HEALTHCHECK`，探测无需鉴权的 `/api/symbols`。
- 按 IP 限流（IPv6 按 /64）：接口每秒 50 次（突发 300），登录每秒 1 次（突发 30），WebSocket 每 IP 最多 100 个；注册每 IP 每小时 2 个、每天 5 个，全站每小时 30 个。放在 Caddy/nginx 后面时**必须**设置 `FX_TRUSTED_PROXY`（地址或网段，如 `172.16.0.0/12` 覆盖 Docker 默认网络），否则所有玩家共用代理那一份额度；此时不要再把 8000 端口暴露到公网。

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

外部行情 smoke test **单独**执行，不能用网络故障替代确定性回归结论：

```sh
cargo run --locked --example market_smoke
```

最新验收记录见 [ACCEPTANCE.md](ACCEPTANCE.md)。

## 兼容与边界

- **已知未修复风险：** 建仓校验未完整扣除新仓即时点差亏损，资金边界或超高杠杆下可能放行资金不足的订单并强平为负余额。当前无负余额保护；仅作模拟实验，不适用于真实资金交易。
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
