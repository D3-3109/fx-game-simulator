# rewrite-rust 计划

状态：基线 `2c7a210`，分支 `rewrite-rust`。阶段 1 静态抽离 `decdc46`、Rust 核心 `653d710`；阶段 2–5 已实现。最终自动门禁：54 Python、Node 图表回归、25 Rust 测试、37 HTTP + 7 WS 双后端配对、真实进程重启与双向互读、fmt/clippy 均通过；外部行情 smoke 独立通过。浏览器 13 项脚本检查与截图通过，但 macOS 锁屏导致前台物理拖动复核尚未完成，不宣称阶段 6 全部验收。启动与证据见 [Rust README](fx-sim-rust/README.md)、[验收记录](fx-sim-rust/ACCEPTANCE.md)。真实服务、数据库及密钥保持不动，生产切换仍未执行。

## 目标与范围

以 Rust 替换当前 Web 后端，保留现有网页、HTTP/WS 协议、SQLite 数据和交易行为。先抽离静态资源，让 Python 与 Rust 使用同一份前端；对照验收完成后再切换根目录。

- 不引入前端框架、打包器、ORM、微服务或多 crate workspace。
- 不顺便改 UI、交易规则、金额精度策略、密码格式或数据库结构。
- CLI (`fx_sim.py`) / Tk GUI (`fx_sim_gui.py`) 暂时保留，不默认纳入 Web 重写，也不默认删除。最终清理前确认去留。
- 每阶段独立提交；验收失败停在当前阶段，不提前删旧实现。

## 已核查的现状

- 已在 `main` 保存基线 `2c7a210`，当前开发分支为 `rewrite-rust`，可从基线恢复旧实现。
- `.gitignore` 已排除数据库及 sidecar、`fx_state.json`、`.pi/`、环境变量文件等。提交前仍需检查 staged diff，不能提交真实账号、token、管理员密钥或备份。
- `fx_sim_web.py`：Python 参照后端；原 `INDEX` 已抽离到 `static/index.html`、`app.css`、`app.js`。
- `fx_sim.py`：交易计算、USD 换算、点差、保证金、强平，以及旧 CLI。
- 管理员页面已抽到 `static/admin.html`、`admin.css`、`admin.js`。
- `static/assets/`：Lightweight Charts 5.2.1 本地兜底、字体及许可证；`static/charts.js` 保留旧 4.2.3 兼容 URL。
- 当前回归：`test_fx_sim.py`、`test_currency.py`、`test_stopout.py`、`test_web_panels.py`、`test_chart.js`。
- 原解析 `web.INDEX` 的测试已改为读取实际外部 JS，原断言保留；新增静态路由安全及 biquote 最新价回归。
- 本机已有 Cargo、rustc 和 Node。基线回归已执行：52 tests，0 failures；后续每阶段重新验证。

## 阶段 0：建立基线与隔离边界

1. 检查忽略规则、源码中的敏感信息；按明确文件清单暂存源码、资源、许可证和测试。
2. 跑现有回归，修复或记录阻塞；通过后保存 `main` 初始基线提交，并记录 commit ID。
3. 从基线执行 `git switch -c rewrite-rust`。
4. 重写测试使用独立端口（例如 8001）、临时数据库和测试管理员配置；不能连接或改写真实账号库。
5. 涉及真实数据的切换前，用 SQLite backup API 生成一致性备份，校验可读性；不能对运行中的数据库仅裸拷主文件。

基线命令：

```sh
/usr/bin/python3 -B -m unittest -v test_fx_sim test_currency test_stopout test_web_panels
```

验收：源码可从 Git 还原；真实数据和密钥未入库；现有回归通过。

## 阶段 1：抽离静态资源，Python 继续运行

先在根目录建立唯一的静态资源目录，不分别给两个后端复制一份。

```text
static/
  index.html
  app.css
  app.js
  admin.html
  admin.css
  admin.js
  charts.js                 # 保留旧 /charts.js 兼容入口
  assets/
    lightweight-charts-5.2.1.js
    noto-sans-latin.woff2
    Lightweight-Charts-LICENSE.txt
    Lightweight-Charts-NOTICE.txt
    OFL-NotoSans.txt
```

实施约束：

- 将 `INDEX` 的 HTML、style、script 原样拆出；管理员页面同样拆分。
- 保留普通脚本及原执行顺序，不改成 ES module，以免破坏现有全局函数、内联事件和启动时序。
- Python 改为从 `static/` 提供 `/`、`/admin`、CSS、JS、字体；使用脚本位置解析目录，不依赖启动工作目录。
- 保持原 `/assets/...`、`/charts.js` URL、MIME、图表版本、SRI、CDN 优先和本地兜底。
- 未带版本的 HTML/app/admin 资源必须可重新验证或不缓存；不套用第三方版本化 JS 的一年 immutable 策略。
- 只允许访问静态目录中的公开资源。路径遍历、编码路径绕过、目录枚举不能暴露 `.pi`、数据库或仓库内容。
- 测试读取 `static/app.js` 后继续交给 `test_chart.js` 的 Node VM；首页路由测试检查外链，再检查外链资源内容。
- 保留字体/图表许可证与署名，不重排业务 JS，不顺便改交互。

验收：原回归通过；新增静态路由、MIME、缓存和越界访问检查；独立浏览器测试页验证登录、图表加载/兜底、拖动、持仓、聊天、排行榜、管理员页。用户真实交易页不动。

## 阶段 2：建立最小 Rust Web 项目

```sh
cargo new fx-sim-rust --vcs none
```

建议技术栈：Axum + Tokio（HTTP/WS）、Serde（JSON）、SQLx（当前使用 SQLite 驱动）；不使用 rusqlite。外部 HTTP/WS 使用成熟客户端，按实现需要添加。随机数、密码验证用现成 crate，不手写加密或 WS 帧协议。具体版本实施时选择并提交 `Cargo.lock`。

数据库未来会迁移，但目标尚未确定：

- 当前继续兼容现有 SQLite，不将语言重写与换库合并。
- SQL、连接池、数据库行映射和事务操作集中在 `storage`；交易计算与 HTTP 层不直接操作 SQLx 的 Row/Pool/Transaction 类型。
- 存储接口按完整业务操作定义，保留账号与成交、充值与审计等原子事务边界，不拆成调用方自行拼接的独立 CRUD。
- 只启用当前所需驱动；不预建多数据库实现、通用 Repository trait 或 SQLx Any 动态后端。
- 若未来选 PostgreSQL/MySQL，可沿用 SQLx，但仍须迁移 SQL 方言、占位符、ID 生成、类型、锁与隔离语义，并搬迁和验证数据；不是换连接串即可完成。SQLx 也不保证支持任意未来数据库。
- 确定目标数据库后，再单独制定 schema/数据迁移、双版本兼容、验收和回滚方案。

- 先做静态页面和服务启动，不一次搭完整分层架构。
- Rust 启动参数显式指定监听地址、数据库、静态目录和管理员配置；开发监听 `127.0.0.1`。
- Python 与 Rust 共用根目录 `static/`；开发时给 Rust 显式传静态目录路径，最终移根不搬第二份资源。
- Rust `target/` 加入忽略规则；构建产物、测试数据库和密钥不提交。
- 初期单 crate；随着移植增加 `trading.rs`、`storage.rs`、`web.rs`、`market.rs` 等必要模块，不预建空抽象层。
- 使用 SQLx 异步接口访问 SQLite；控制 SQLite 写并发与事务范围，事务中不等待外部行情请求。

验收：Rust 可在独立端口提供同一页面；缺失配置、非法路径和启动错误明确报错；`cargo check`、基础路由测试通过。

## 阶段 3：移植交易核心与持久化

先移植纯计算，再接事务。使用固定报价、时钟及脱敏 fixture，与 Python 结果对照。

必须保持：

- 每笔持仓保存币对，平仓/保证金/强平使用持仓币对，不使用当前选择币对。
- 旧仓缺币对时明确阻止结算，不猜、不补默认币对。
- 余额、保证金、盈亏统一 USD；JPY 等报价币需对应美元换算价，缺价不得部分结算。
- 点差、最小手数、杠杆边界、50% 强平线、按 USD 亏损排序逐仓强平保持一致。
- 浮点输入必须有限，拒绝 NaN/Infinity；本轮保留现有双精度计算口径，不附带 Decimal 迁移。
- 行情缺失、过期、乱序不误触发结算；无需浏览器在线也能强平。
- 账号、持仓、成交记录在同一事务提交；失败时内存与数据库同时恢复；并发报价不能重复强平。
- 保留 account JSON 字段、position ID/next_id 语义、普通账号 token 与已有密码校验兼容性。
- 保留 `trades.currency` 的 USD/LEGACY 区分；不改写旧余额与旧成交口径。

数据库核对范围：`users`、`trades`、`chat`、`deposits`、`symbol_settings`、`equity_samples`、`admin_events`、`sim_bars`，以及旧 `fx_state.json` 的实际读写用途。使用脱敏副本检查所有读写者，首次启动不得重置或覆盖已有记录。

验收：对等 Rust 核心测试、Python/Rust 固定输入差分测试、事务失败/并发测试、重启恢复测试；证明 Rust 写入的数据仍能由旧 Python 读取。真实服务不参与对照写入。

## 阶段 4：补齐 HTTP、管理员与 WebSocket

按功能逐组移植，不只实现首页能打开的最小子集。

| 范围 | 现有接口 |
| --- | --- |
| 账号 | POST `/api/auth`；GET `/api/me` |
| 交易 | GET `/api/state`、`/api/history`；POST `/api/trade`、`/api/close`、`/api/leverage`、`/api/symbol` |
| 行情/币种 | GET `/api/klines`、`/api/symbols` |
| 社交 | GET `/api/leaderboard`、`/api/chat`；POST `/api/chat` |
| 管理员读取 | GET `/api/admin/users`、`/api/admin/deposits`、`/api/admin/symbols` |
| 管理员写入 | POST `/api/admin/login`、`/api/admin/logout`、`/api/admin/deposit`、`/api/admin/delete`、`/api/admin/symbols` |
| 实时推送 | `/ws?token=...` |

- 保持请求字段、响应字段、HTTP 状态、错误结构、秒/毫秒单位、null/空列表和鉴权语义；非法输入安全拒绝，不复刻意外崩溃。
- WS 保留 `state`、`lb`、`chat`、`symbols`，以及当前无 `type` 的报价包；FXSIM 报价携带完整 OHLC 和时间戳。
- 保留多标签在线判定、断线清理、账号隔离；删除账号撤销 token 和连接。
- 管理员 PSK 校验、会话过期、限流和登出保持；旧内存管理员会话切换后可要求重新登录，普通持久化账号 token 不丢失。
- 充值按 request_id 幂等并保留审计；排行榜与五分钟收益扣除充值，数据不足时不能编造五分钟收益。
- 禁用币种只禁新开仓，不能阻断平仓、强平或 USD 换算行情。
- 限制请求体和输入长度；静态页面公开不等于管理员 API 公开；日志不输出 token、PSK 或敏感配置。

验收：用黑盒 HTTP/WS 契约测试分别跑两个后端；现有 Python 内部测试不能直接当作 Rust 已通过的证据。管理员失败回滚、充值重试、删除撤权、聊天转义、断连/重连均有覆盖。

## 阶段 5：移植行情与真实 K 线持久化

- 保留 Binance、biquote、Yahoo 对应币种策略、超时/回退、代理设置及所持币种订阅。
- 保留后台报价、风险检查、每用户状态和排行榜推送；断线重连不泄漏任务、不重复结算。
- FXSIM 基准 1.1000、约 0.2 秒 tick；支持现有八种 K 线周期。
- K 线只来自实际记录；完成周期冻结、每周期最多 1000 根；不随机补历史，不补停服空档。
- REST 与 WS 保持时间戳/完整 OHLC 合并语义；刷新或重启不能改写已闭合蜡烛或丢失影线。
- 对照使用固定 tick 输入，不要求两种随机数生成器产生完全相同的未来行情。

验收：固定 tick 跨周期测试、SQLite 重启测试、REST/WS 乱序及极值测试；外部行情做独立 smoke test，网络故障与代码回归分开报告。

## 阶段 6：并行验收与切换

并行运行仅指两个服务使用不同端口、不同数据库做对照，绝不共享同一可写账户库。旧实现有内存状态，共享 SQLite 会造成互相覆盖。

切换门槛：

- 所有承诺功能、核心回归、黑盒 HTTP/WS 和浏览器验收通过。
- 数据恢复、失败回滚、并发交易、行情断线、停止/重启均有证据。
- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 通过。
- 浏览器只用 Kimi WebBridge 独立测试页；检查图表时将测试页置前台，避免后台 resize 暂停影响结论。
- 明确保留/删除文件清单；CLI/GUI 去留未确认前不删除。

最终操作另行确认后执行：

1. 停止已确认的旧服务进程，确保端口释放；此后禁止双写。
2. 对停服时最终数据库做一致性备份并验证；保留密钥及配置，不输出其内容。
3. 将 `fx-sim-rust/Cargo.toml`、`Cargo.lock`、`src/`、Rust 测试等明确路径移到根目录；`static/` 已在根目录，无需重复搬运；`target/` 不作为交付迁移。
4. 更新启动说明、资源/数据路径，重新跑验收；源码搬家与运行数据迁移分开。
5. Rust 使用原数据库路径和原端口启动，保留浏览器同源与 localStorage token；确认账号、余额、持仓、历史和 K 线恢复。
6. 稳定验证后按清单移除已替代的旧 Web 实现与失效测试，单独做切换/清理提交。保留文档、许可证、Git、运行数据及必要配置。

回滚：停止 Rust，先确认当前数据库仍兼容旧 Python，再用记录的旧版本和正确静态资源恢复服务。切换后若已有新交易，不能直接覆盖成旧备份丢掉新数据；兼容检查失败时先停写、保存当前库，再制定数据恢复方案。

## 下一步

实现及可自动执行的隔离验收已完成，代码/测试/启动说明/验收记录提交在 `rewrite-rust`。剩余非破坏性验收：解锁 macOS 后，仅在独立测试页补前台真实拖动复核；锁屏限制及已有替代证据如实保留于验收记录。最终生产切换、根目录搬迁和旧实现删除仍另行确认。FX provider 不变；biquote 已按最新活动 K 线 close 取价，不宣称逐笔实时。
