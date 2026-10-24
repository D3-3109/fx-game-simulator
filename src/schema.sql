CREATE TABLE IF NOT EXISTS users (
    name TEXT PRIMARY KEY, salt TEXT, hash TEXT, token TEXT,
    symbol TEXT DEFAULT 'USDJPY', account TEXT, created INTEGER
);
CREATE TABLE IF NOT EXISTS trades (
    id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL,
    time INTEGER NOT NULL, kind TEXT NOT NULL, position_id INTEGER,
    symbol TEXT, side TEXT, lots REAL, entry REAL, price REAL,
    pnl REAL, balance REAL, margin_level REAL, currency TEXT DEFAULT 'LEGACY'
);
CREATE INDEX IF NOT EXISTS trades_by_user ON trades(name,id DESC);
CREATE TABLE IF NOT EXISTS chat (
    id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL,
    time INTEGER NOT NULL, text TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS deposits (
    request_id TEXT PRIMARY KEY, name TEXT NOT NULL, time INTEGER NOT NULL,
    amount REAL NOT NULL, balance REAL NOT NULL, actor TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS symbol_settings (
    symbol TEXT PRIMARY KEY, enabled INTEGER NOT NULL CHECK(enabled IN (0,1)), version INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS equity_samples (
    name TEXT NOT NULL, time INTEGER NOT NULL, equity REAL NOT NULL,
    deposited REAL NOT NULL, PRIMARY KEY(name,time)
);
CREATE TABLE IF NOT EXISTS admin_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, name TEXT NOT NULL,
    time INTEGER NOT NULL, request_id TEXT UNIQUE, detail TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS registrations (
    client TEXT NOT NULL, time INTEGER NOT NULL, name TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS registrations_by_client ON registrations(client,time);
CREATE INDEX IF NOT EXISTS registrations_by_time ON registrations(time);
CREATE TABLE IF NOT EXISTS sim_bars (
    symbol TEXT NOT NULL DEFAULT 'FXSIM', sec INTEGER, t INTEGER, o REAL, h REAL, l REAL, c REAL,
    PRIMARY KEY(symbol,sec,t)
);
