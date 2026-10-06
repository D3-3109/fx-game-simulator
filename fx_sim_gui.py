#!/Users/ayami/.local/share/uv/python/cpython-3.13-macos-aarch64-none/bin/python3
"""外汇保证金模拟器 GUI (tkinter). 复用 fx_sim 的账户逻辑.

Run:
  ~/.local/share/uv/python/cpython-3.13-macos-aarch64-none/bin/python3 fx_sim_gui.py
  # 或 /usr/bin/python3 fx_sim_gui.py
"""
from __future__ import annotations
import threading
import time
import tkinter as tk
from tkinter import ttk

import fx_sim
from fx_sim import Account, STOPOUT, fetch_price, pip_size

SYMBOLS = ["EURUSDT", "GBPUSDT", "USDJPY", "AUDUSDT", "USDCNH"]
REFRESH_MS = 1000


class App:
    def __init__(self, root: tk.Tk):
        self.root = root
        root.title("FX 保证金模拟器")
        root.geometry("760x560")

        self.price = None
        self.price_err = None
        self.symbol = tk.StringVar(value="EURUSDT")
        self.leverage = tk.IntVar(value=100)
        self.lots = tk.StringVar(value="0.10")
        self.acc: Account | None = None
        self._alive = True

        self._build()
        self._log("点 [连接行情] 开始")

        self._fetcher = threading.Thread(target=self._loop, daemon=True)
        self._fetcher.start()
        self._tick()
        root.protocol("WM_DELETE_WINDOW", self._quit)

    # ---------- UI ----------
    def _build(self):
        top = ttk.Frame(self.root, padding=6)
        top.pack(fill="x")
        ttk.Label(top, text="交易对").pack(side="left")
        cb = ttk.Combobox(top, textvariable=self.symbol, values=SYMBOLS,
                          width=10, state="readonly")
        cb.pack(side="left", padx=4)
        ttk.Button(top, text="连接行情", command=self._connect).pack(side="left", padx=4)
        ttk.Label(top, text="代理").pack(side="left", padx=(12, 0))
        self.proxy = tk.StringVar(value=fx_sim.PROXY or "")
        ttk.Entry(top, textvariable=self.proxy, width=22).pack(side="left", padx=4)
        ttk.Label(top, text="杠杆 1:").pack(side="left", padx=(12, 0))
        spin = ttk.Spinbox(top, from_=1, to=1000, textvariable=self.leverage, width=5)
        spin.pack(side="left")

        self.price_lbl = tk.Label(self.root, text="--", font=("Menlo", 28, "bold"))
        self.price_lbl.pack(pady=2)
        self.chg_lbl = tk.Label(self.root, text="", font=("Menlo", 10))
        self.chg_lbl.pack()

        acct = ttk.LabelFrame(self.root, text="账户", padding=6)
        acct.pack(fill="x", padx=8)
        self.acct_lbl = tk.Label(acct, text="--", font=("Menlo", 11), justify="left", anchor="w")
        self.acct_lbl.pack(fill="x")

        trade = ttk.LabelFrame(self.root, text="交易", padding=6)
        trade.pack(fill="x", padx=8, pady=4)
        ttk.Label(trade, text="手数").pack(side="left")
        ttk.Entry(trade, textvariable=self.lots, width=7).pack(side="left", padx=4)
        ttk.Button(trade, text="买入 / 多", command=lambda: self._open("long")).pack(side="left", padx=4)
        ttk.Button(trade, text="卖出 / 空", command=lambda: self._open("short")).pack(side="left", padx=4)
        ttk.Button(trade, text="全部平仓", command=self._close_all).pack(side="left", padx=12)

        cols = ("id", "side", "lots", "entry", "now", "pnl", "margin")
        pf = ttk.LabelFrame(self.root, text="持仓 (双击平仓)", padding=4)
        pf.pack(fill="both", expand=True, padx=8, pady=4)
        self.tree = ttk.Treeview(pf, columns=cols, show="headings", height=6)
        widths = (40, 60, 70, 90, 90, 90, 90)
        for c, w in zip(cols, widths):
            self.tree.heading(c, text=c.upper() if c != "entry" else "开仓价")
            self.tree.column(c, width=w, anchor="e" if c not in ("side",) else "center")
        sb = ttk.Scrollbar(pf, orient="vertical", command=self.tree.yview)
        self.tree.configure(yscrollcommand=sb.set)
        self.tree.pack(side="left", fill="both", expand=True)
        sb.pack(side="right", fill="y")
        self.tree.bind("<Double-1>", self._on_row_dbl)

        self.log_txt = tk.Text(self.root, height=6, font=("Menlo", 10), state="disabled")
        self.log_txt.pack(fill="x", padx=8, pady=(0, 8))

    def _log(self, msg: str):
        self.log_txt.configure(state="normal")
        self.log_txt.insert("end", f"{time.strftime('%H:%M:%S')}  {msg}\n")
        self.log_txt.see("end")
        self.log_txt.configure(state="disabled")

    # ---------- 行情线程 ----------
    def _loop(self):
        while self._alive:
            sym = getattr(self, "cur_sym", None)
            if sym:
                try:
                    self.price = fetch_price(sym)
                    self.price_err = None
                except Exception as e:
                    self.price_err = str(e)
            time.sleep(1)

    def _connect(self):
        fx_sim.PROXY = self.proxy.get().strip() or None
        sym = self.symbol.get()
        try:
            p = fetch_price(sym)
        except Exception as e:
            self._log(f"连接失败: {e}")
            return
        if self.acc is None or self.cur_sym != sym:
            try:
                lev = max(1, int(self.leverage.get()))
            except Exception:
                lev = 100
            self.acc = Account(cash=10000.0, leverage=lev)
            self._log(f"新账户 {sym} 余额 10000 杠杆 1:{lev}")
        self.cur_sym = sym
        self.price = p
        self.pip = pip_size(sym)
        self._log(f"已连接 {sym} @ {p:.5f}")

    # ---------- 交易 ----------
    def _need(self) -> bool:
        if self.acc is None or not getattr(self, "cur_sym", None) or self.price is None:
            self._log("先连接行情")
            return False
        return True

    def _open(self, side: str):
        if not self._need():
            return
        try:
            lots = float(self.lots.get())
            self.acc.open(side, lots, self.price)
            self._check_stopout()
        except ValueError as e:
            self._log(f"err: {e}")

    def _close_pos(self, pid: int):
        if not self._need():
            return
        try:
            self.acc.close(pid, self.price)
        except ValueError as e:
            self._log(f"err: {e}")

    def _close_all(self):
        if not self._need():
            return
        try:
            self.acc.close(None, self.price)
        except ValueError as e:
            self._log(str(e))

    def _on_row_dbl(self, _e):
        sel = self.tree.selection()
        if sel:
            self._close_pos(int(sel[0]))

    def _check_stopout(self):
        if self.acc and self.acc.check_stopout(self.price):
            self._log("触发强制平仓检查")

    # ---------- 刷新 ----------
    def _tick(self):
        if not self._alive:
            return
        # 价格
        if getattr(self, "price_err", None):
            self.chg_lbl.configure(text=f"行情错误: {self.price_err}", fg="#b00")
        elif self.price is not None:
            self.price_lbl.configure(text=f"{self.price:.5f}")
            self._render_acct()
            self._render_positions()
        self.root.after(REFRESH_MS, self._tick)

    def _render_acct(self):
        a = self.acc
        if a is None:
            return
        eq = a.equity(self.price)
        used = a.used_margin(self.price)
        ml = a.margin_level(self.price)
        ml_s = f"{ml*100:.0f}%" if ml else "-"
        color = "#0a0" if ml is None or ml > 100 else ("#d80" if ml > STOPOUT * 100 else "#d00")
        self.acct_lbl.configure(fg=color if ml else "#000", text=(
            f"余额 {a.balance:>10.2f}   净值 {eq:>10.2f}   已用保证金 {used:>8.2f}\n"
            f"可用 {eq - used:>10.2f}   强平线 {ml_s:>7}   杠杆 1:{a.leverage}"))

    def _render_positions(self):
        a = self.acc
        if a is None:
            return
        rows = {str(p.id): p for p in a.positions}
        for iid in list(self.tree.get_children()):
            p = rows.get(iid)
            if p is None:
                self.tree.delete(iid)
                self.tree.set(iid, "now", f"{self.price:.5f}")
                pnl = p.pnl(self.price)
                self.tree.set(iid, "pnl", f"{pnl:+.2f}")
                self.tree.item(iid, tags=("win" if pnl >= 0 else "loss",))
        for iid, p in rows.items():
            if not self.tree.exists(iid):
                self.tree.insert("", "end", iid=iid, values=(
                    p.id, "多" if p.side == "long" else "空", f"{p.lots:g}",
                    f"{p.entry:.5f}", f"{self.price:.5f}",
                    f"{p.pnl(self.price):+.2f}",
                    f"{p.margin(self.price, a.leverage):.2f}"))

    def _quit(self):
        self._alive = False
        self.root.destroy()


def main():
    root = tk.Tk()
    App(root)
    root.mainloop()


if __name__ == "__main__":
    main()
