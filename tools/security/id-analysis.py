#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""ID 随机性与注册凭证实测。

S1  --id 抽样 N 次（每次全新配置目录），统计位数/唯一性/首位分布/数字频率
S2  全新配置目录起被控端，用 rdterm-id.json 里的 12 位 ID 走 rendezvous 连接，
    成功即可判定「注册到 ID 服务器的是 12 位随机 ID」，而非 MAC 派生的 9 位 ID

用法: python id-analysis.py [样本数，默认 30] [test|sample|all]
"""
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import time
from collections import Counter

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

ROOT = r"D:\rdterm"
EXE = os.path.join(ROOT, "rdterm.exe")
TMP = os.path.join(ROOT, "_idprobe")


def sample(n=30):
    print(f"=== S1 ID 抽样（{n} 次，每次全新配置目录）===")
    ids = []
    for i in range(n):
        d = os.path.join(TMP, str(i))
        shutil.rmtree(d, ignore_errors=True)
        try:
            p = subprocess.run([EXE, "--id", "--config-dir", d], capture_output=True, timeout=25)
            out = p.stdout.decode("utf-8", "ignore") + p.stderr.decode("utf-8", "ignore")
        except subprocess.TimeoutExpired:
            continue
        m = re.search(r"ID\s*:\s*(\d+)", out)
        if m:
            ids.append(m.group(1))
    if not ids:
        print("  未取到样本")
        return
    lens = Counter(len(x) for x in ids)
    firsts = Counter(x[0] for x in ids)
    all_digits = Counter("".join(ids))
    dup = len(ids) - len(set(ids))
    print(f"  样本数: {len(ids)}  唯一: {len(set(ids))}  重复: {dup}")
    print(f"  位数分布: {dict(lens)}")
    print(f"  首位分布: {dict(sorted(firsts.items()))}   （首位为 0 的次数应为 0）")
    print(f"  数字频率(0-9): {[all_digits.get(str(i), 0) for i in range(10)]}  期望≈{len(''.join(ids))/10:.1f}")
    print(f"  样本: {ids[:6]} …")
    print(f"  空间估算: 12 位且首位非 0 → 9×10^11 ≈ {9e11:.3e}，熵 ≈ "
          f"{(9 * 10**11).bit_length() - 1} bit（对比 MAC 派生的 29 bit 自动 ID）")
    return ids


def register_check():
    print("\n=== S2 注册凭证判定（走 rendezvous）===")
    cfg = os.path.join(ROOT, "_sec_srv_reg")
    shutil.rmtree(cfg, ignore_errors=True)
    os.makedirs(cfg, exist_ok=True)
    p = subprocess.Popen([EXE, "--server", "--hide", "--config-dir", cfg],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    time.sleep(10)
    try:
        with open(os.path.join(cfg, "rdterm-id.json"), encoding="utf-8") as f:
            rid = json.load(f)["id"]
    except Exception as e:
        print(f"  读取 ID 失败: {e}")
        p.kill()
        return
    print(f"  被控端 rdterm-id.json 中的 ID = {rid}（位数 {len(rid)}）")
    spec = importlib.util.spec_from_file_location("mc", os.path.join(ROOT, "mcp-connect.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    ctl = os.path.join(ROOT, "_sec_ctl_reg")
    shutil.rmtree(ctl, ignore_errors=True)
    m = mod.Mcp(cwd=os.path.join(ROOT, "_ctl"))
    try:
        try:
            m.p.kill()
        except Exception:
            pass
        pp = subprocess.Popen([EXE, "--mcp", "--config-dir", ctl],
                              cwd=os.path.join(ROOT, "_ctl"), stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        m.p = pp
        m.rid = 0
        m.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                              "clientInfo": {"name": "id-check", "version": "1.0"}})
        m.notify("notifications/initialized")
        err, txt = m.tool("rdterm_connect", {"peer": rid, "rows": 40, "cols": 200}, timeout=90)
        print(f"  按 12 位 ID 连接: err={err} {txt[:120]}")
        if not err:
            sid = json.loads(txt)["session"]
            err2, out = m.tool("rdterm_exec", {"session": sid, "command": "hostname", "wait_ms": 8000})
            print(f"  取到输出: {out.strip()[:80]!r}")
            m.tool("rdterm_close", {"session": sid})
            print("  >>> 结论：ID 服务器上登记的正是这个 12 位随机 ID（注册凭证 = 随机 ID）")
        else:
            print("  >>> 连接失败：需人工确认是否登记成了 MAC 派生的 9 位 ID")
    finally:
        m.close()
        p.kill()


if __name__ == "__main__":
    what = sys.argv[2] if len(sys.argv) > 2 else "all"
    n = int(sys.argv[1]) if len(sys.argv) > 1 and sys.argv[1].isdigit() else 30
    if what in ("sample", "all"):
        sample(n)
    if what in ("test", "all"):
        register_check()
