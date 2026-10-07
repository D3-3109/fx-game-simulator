# Rust rewrite 验收记录

分支：`rewrite-rust`。范围为计划阶段 1–5、阶段 6 的隔离测试；**不是生产切换记录**。

## 已通过的门禁

| 检查 | 结果 / 可复现入口 |
| --- | --- |
| Python 原回归 + 新静态/biquote 回归 | 54 tests，0 failures；`python3 -B -m unittest -v test_fx_sim test_currency test_stopout test_web_panels` |
| Node 执行实际 `static/app.js` | 通过；切换/过期状态、OHLC/影线、闭合蜡烛、刷新、面板拖拽与尺寸、CDN/SRI/兜底等 |
| Rust | 19 单元 + 1 差分 + 5 HTTP/WS 集成测试，0 failures / ignored |
| 固定输入差分 | 18 场景，对比 Python 和 Rust USD 结算、点差、保证金、强平、缺价及参数边界 |
| 双后端黑盒契约 | `tests/blackbox_contract.py`：37 配对 HTTP 操作 + 7 配对 WS 包；各用独立数据库/临时端口 |
| 数据兼容 | Python schema/密码/token/旧 currency 迁移读取；Rust 写回后 Python 可读；原子失败回滚、并发唯一强平、充值幂等、删除审计保留 |
| 实际进程重启 | `tests/process_restart.py`：不同 CWD 启动、带 WS 的 SIGINT、端口释放、重启保留 token/余额/持仓/历史/聊天/充值/审计/白名单/闭合影线，最后 Python 重新加载 |
| 行情确定性 | 最新 biquote bar（两种数组顺序、分钟内更新）；过期/缺失换算/乱序不误强平；8 周期 OHLC、1000 根上限、无伪造空档、SQLite 重启；假上游验证共享 Binance 连接、重连和重订阅 |
| 静态安全 | MIME/cache/旧 URL；固定公开清单；路径遍历/编码越界不可读数据库与密钥；独立启动参数 |
| Rust 格式/静态检查 | `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 通过 |

命令及依赖见 [README](README.md)。确定性测试不依赖外部 provider。

## 外部行情 smoke（与上述门禁分开）

`cargo run --locked --manifest-path fx-sim-rust/Cargo.toml --example market_smoke` 实测通过：

- USDJPY biquote 最新活动 close、Yahoo regularMarketPrice 返回有效正价。
- USDJPY biquote 历史 OHLC、BTCUSDT Binance REST 历史可用。
- Binance TLS WS 收到报价。

这是当次可达性证据，不是全天 SLA / 精确实时延迟保证；没有切到 Tiingo，也未使用其 key。

## 隔离浏览器证据

Kimi WebBridge 独立标签页，`127.0.0.1:8001`，测试用户 `rust-browser`，测试库 `.pi/rust-acceptance/test.db`。没有操作原 8000 的交易标签页。

已实测：注册/登录、USDJPY 图表、FXSIM 1m 实际影线、开/平仓及持仓线、成交记录、聊天 HTML 转义、排行；管理员登录、充值审计、禁用/启用币种及 WS 按钮同步、测试账号删除、登出。共 13 项浏览器脚本检查通过。

- 使用真实 Lightweight Charts 5.2.1；正常 CDN 载入通过。
- 同源隔离 iframe 施加 `script-src 'self' 'unsafe-inline'` 阻断 CDN，验证原加载器回退到本地带 SRI 的 5.2.1；绘制 7 个 canvas。
- 键盘分隔条调整：容器高度 522→496；之后实测 `chart.options().height === cv.clientHeight === 496`，手动价格轴缩放仍保留。
- 完整页面 reload 后，17 根既有闭合 FXSIM 1m 蜡烛 OHLC 完全一致，测试登录保留。
- 已读取截图检查图表、持仓线、中文界面、聊天、管理员页和本地兜底。

本机证据（忽略的 `.pi/` 中，不含真实账户）：

- `rust-acceptance/browser-evidence.json`、`full-page-reload.json`
- `rust-acceptance/fxsim-trade.png`、`local-fallback.png`、`admin.png`
- `rust-final-tests.log`、`python-final-tests.log`、`node-final-tests.log`
- `blackbox-contract-final.log`、`process-restart-final.log`、`market-smoke-final.log`

### 前台物理拖动复核：已通过（2026-10-07）

验收时 macOS 屏幕锁定曾导致脚本标签 `visibilityState='hidden'`，前台拖动当时无法自动执行；Node 拖拽回归、键盘 resize 与截图先行通过。随后由用户在解锁桌面手动复核：`127.0.0.1:8001` 页面内鼠标拖动图表/面板分隔条，高度实时跟随、布局无破损、价格轴缩放与持仓线保持正常。此项按用户实测结论标记完成。

## 保留项、差异与回滚

- 旧 Python 实现、CLI/Tk GUI、真实数据库和密钥保留，原 8000 服务未重启。新源码中的 biquote 修复不会热更新已运行的旧 Python 进程。
- 静态源码唯一为 `static/`。为不破坏**已运行的旧版** Python 进程，工作区另留被忽略的 `assets`/`charts.js` 符号链接及基线生成的 `admin.html` 临时兼容文件；它们不提交、不是新开发资源源头。旧进程获准退出后即可移除，新 Python/Rust 均不依赖它们。
- 新生成 position ID 不复制 Python 惰性加载产生的空洞；既有 ID 和引用保持。非确定时间/token/新 ID 映射见黑盒测试。输入安全拒绝与数据写失败明确报错，不复刻旧服务意外异常。
- 不更换 provider、不切换真实数据、不搬根目录、不删除旧源码。未验证多实例共写、负载/SLA 或真实库切换，未宣称支持。
- 最终切换仍按 `REWRITE-RUST.md` 另行确认：停止旧服务→一致性备份→逐路径搬迁→复验→用原数据库/端口启动→再清理旧 Web。回滚不能用旧备份覆盖切换后新增交易。
