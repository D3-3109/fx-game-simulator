# FX simulator — Rust Web backend

Axum/Tokio + SQLx SQLite. 单 crate，直接使用 `../static/`，无需前端构建。
Python Web / CLI / Tk GUI 保留作为兼容参照；**未切换原服务，也未搬到仓库根目录**。

## 独立启动

在仓库根目录运行（已验证 Rust 1.98.1）：

```sh
cargo build --locked --manifest-path Cargo.toml
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

./target/debug/fx-sim-rust \\
  --listen 127.0.0.1:8001 \\
  --db "$PWD/.pi/rust-dev/test.db" \\
  --static-dir "$PWD/static" \\
  --admin-key "$PWD/.pi/rust-dev/admin.json"
```

打开 `http://127.0.0.1:8001`，注册测试用户；管理员页面 `/admin`。Ctrl+C 关闭连接、后台任务与数据库。静态目录、数据库、管理员文件均为显式参数，支持任意工作目录；默认监听回环 8001。目录须预先存在。

**不要将 Python 和 Rust 指向同一个可写数据库。** Python 有内存账户状态，双写会覆盖交易。切换原 8000 服务、真实数据库或旧文件清理必须单独确认，见 [计划](REWRITE-RUST.md)。账户 token 存在浏览器 origin 下，换端口不会自动继承旧登录。

## 验证

需要 Node、Python 3.11+；网络无关的 Python/Rust 黑盒 WS 测试另需 `websockets>=15`（可在独立 venv 安装）。普通 Python 后端无第三方依赖。

从仓库根目录执行：

```sh
python3 -B -m unittest -v test_fx_sim test_currency test_stopout test_web_panels
node test_chart.js < static/app.js
cargo fmt --manifest-path Cargo.toml --check
cargo clippy --locked --manifest-path Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path Cargo.toml
cargo build --locked --manifest-path Cargo.toml
cargo build --locked --manifest-path Cargo.toml --example fixture_server
python3 -B tests/blackbox_contract.py
python3 -B tests/process_restart.py
```

所有测试使用临时数据库/回环临时端口/测试管理员配置，不读取真实用户数据或 Tiingo key。`fixture_server` 是测试工具，不是生产入口。

外部行情 smoke test **单独**执行，不能用网络故障替代确定性回归结论：

```sh
cargo run --locked --manifest-path Cargo.toml --example market_smoke
```

最新验收记录见 [ACCEPTANCE.md](ACCEPTANCE.md)。

## 兼容与边界

- FX 保留 biquote 主源、Yahoo fallback 与后台轮询；Binance 共享上游 WS 广播，不是每个浏览器一条上游。FXSIM 本地真实 tick 聚合。
- biquote `limit=1` 可能返回两根，按最大 `openTime` 取最新活动根的 `close`，不是 `bars[-1]`；Python 参照同步修复。它是分钟 OHLC 内持续更新的近实时价格，不保证逐笔/毫秒实时。
- 保留 8 秒取价缓存及取价失败时旧缓存回退；自动风险检查拒用超过 30 秒的缓存或缺少 USD 换算价。刷新时间是本机收取时间，不是上游逐笔时间；未新增固定延迟。
- REST 遵循 `FX_PROXY` / HTTPS/HTTP 代理环境；Binance WS 与原 Python 一样直连 TLS。不要把含凭据的代理 URL 打入日志。
- `storage.rs` 独占 SQL 和完整业务事务。SQLite 单连接 + 全局写锁保证当前单进程模型的原子性；没有多实例写入或吞吐保证，也没有预建通用数据库框架。
- 保留旧 schema、账户 JSON、普通账号密码算法/token、USD/LEGACY 历史；旧 `trades` 缺 `currency` 时补列标为 LEGACY，不改旧金额。`fx_state.json` 属于旧单用户辅助路径，当前多用户 Web 使用 SQLite；Rust 不读取/覆盖它。
- 新生成的 position ID 唯一并可恢复，但不保证复制 Python 惰性载入造成的编号空洞；已保存 ID 原样保留。契约对照只对新 ID 做稳定引用映射，时间/token 等非确定值不逐字比较。
- 普通登录按旧行为轮换持久 token，旧登录 token 在本进程内暂存直至重启；管理员会话仅内存保留，重启需重新登录。
- 交易/审计写失败返回明确错误且回滚；非法 JSON、过大请求等安全拒绝，不复刻 Python 的意外崩溃。模拟随机数不要求和 Python 逐步相同，真实 OHLC 聚合/持久化相同。
- 静态资源只开放固定清单，保留旧 `/charts.js`、字体/图表许可证和 5.2.1 SRI/CDN 兜底。HTTP 本身不加密，仅供本机/可信网络模拟用途。
