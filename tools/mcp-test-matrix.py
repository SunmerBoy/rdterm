#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""rdterm MCP 全链路测试矩阵（结果写入 mcp-test-report.md）"""
import importlib.util
import json
import os
import sys
import time

_self = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("mcp_connect", os.path.join(_self, "mcp-connect.py"))
_mcp_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mcp_mod)
Mcp = _mcp_mod.Mcp

PEER = sys.argv[1] if len(sys.argv) > 1 else "805307194538"
REPORT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "mcp-test-report.md")
results = []


def record(name, ok, detail, elapsed):
    results.append((name, ok, elapsed, detail))
    print(f"[{'PASS' if ok else 'FAIL'}] {name} ({elapsed:.1f}s) :: {detail[:120].replace(chr(10), ' | ')}")


def run(m, sid, name, cmd, wait_ms=15000, check=None, sample=None):
    t0 = time.time()
    err, out = m.tool("rdterm_exec", {"session": sid, "command": cmd, "wait_ms": wait_ms})
    el = time.time() - t0
    ok = not err
    detail = out
    if ok and check:
        ok = check(out)
        if not ok:
            detail = "CHECK FAILED:\n" + out
    elif ok and sample:
        detail = out[:sample]
    record(name, ok, detail, el)
    return out


def main():
    m = Mcp()
    try:
        # T0 握手
        t0 = time.time()
        r = m.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                  "clientInfo": {"name": "test-matrix", "version": "1.0"}})
        m.notify("notifications/initialized")
        info = r.get("result", {}).get("serverInfo", {})
        record("T0 initialize 握手", bool(info.get("name")), json.dumps(r.get("result", {}), ensure_ascii=False), time.time() - t0)

        # 本机身份
        err, txt = m.tool("rdterm_identity")
        record("T0b rdterm_identity", not err, txt, 0)

        # T1 连接
        t0 = time.time()
        err, txt = m.tool("rdterm_connect", {"peer": PEER, "rows": 50, "cols": 200}, timeout=90)
        record("T1 connect", not err, txt, time.time() - t0)
        if err:
            return
        sid = json.loads(txt)["session"]

        # T2 基础命令
        run(m, sid, "T2a hostname", "hostname", check=lambda o: o.strip() != "")
        run(m, sid, "T2b echo 哨兵", "echo ABC123XYZ", check=lambda o: "ABC123XYZ" in o)
        run(m, sid, "T2c 管道", "powershell -c \"Get-Process | Measure-Object | Select-Object -ExpandProperty Count\"",
            check=lambda o: o.strip().splitlines()[-1].strip().isdigit() if o.strip() else False)

        # T3 长输出
        run(m, sid, "T3a ipconfig /all（长输出完整性）", "ipconfig /all", wait_ms=15000,
            check=lambda o: ("IPv4" in o or "IP Address" in o or "IPv6" in o) and "DNS" in o.upper())

        # T4 特殊字符
        run(m, sid, "T4a 引号嵌套", "powershell -c \"Write-Output ('quote:' + [char]34 + 'inner' + [char]34)\"",
            check=lambda o: "quote:" in o)
        run(m, sid, "T4b 美元/变量(单引号防外层展开)", "powershell -c '$v=41+1; Write-Output RESULT=$v'",
            check=lambda o: "RESULT=42" in o)

        # T5 会话状态保持（连续两条）
        run(m, sid, "T5a cd 设置状态", "cd C:\\Windows", wait_ms=8000)
        run(m, sid, "T5b 状态是否保持", "powershell -c \"(Get-Location).Path\"",
            check=lambda o: "Windows" in o)

        # T6 长命令 + 轮询
        run(m, sid, "T6a 长命令启动(ping)", "ping -n 6 127.0.0.1", wait_ms=2000)
        got = ""
        for _ in range(12):
            err, txt = m.tool("rdterm_read", {"session": sid})
            got += txt
            if "平均" in got or "Average" in got or "Minimum" in got or "最短" in got:
                break
            time.sleep(1)
        record("T6b 长命令轮询收尾", ("127" in got and ("平均" in got or "Average" in got or "Minimum" in got)), got, 12)

        # T7 错误命令
        run(m, sid, "T7 错误命令处理", "this_command_does_not_exist_xyz", check=lambda o: True)

        # T8 中文/编码
        run(m, sid, "T8 中文输出", "powershell -c \"Write-Output 中文测试abc\"", check=lambda o: "中文测试" in o)

        # T9 resize
        t0 = time.time()
        err, txt = m.tool("rdterm_resize", {"session": sid, "rows": 30, "cols": 120})
        record("T9 resize", not err, txt, time.time() - t0)

        # T10 多会话并发
        err, txt = m.tool("rdterm_connect", {"peer": PEER, "rows": 24, "cols": 120}, timeout=90)
        sid2_ok = not err
        record("T10a 第二会话建立", sid2_ok, txt, 0)
        if sid2_ok:
            sid2 = json.loads(txt)["session"]
            err, o1 = m.tool("rdterm_exec", {"session": sid, "command": "echo FROM-SESSION-1", "wait_ms": 8000})
            err, o2 = m.tool("rdterm_exec", {"session": sid2, "command": "echo FROM-SESSION-2", "wait_ms": 8000})
            record("T10b 双会话隔离", ("FROM-SESSION-1" in o1) and ("FROM-SESSION-2" in o2), f"S1:{o1[:80]}\nS2:{o2[:80]}", 0)
            m.tool("rdterm_close", {"session": sid2})

        # T11 会话列表
        err, txt = m.tool("rdterm_sessions")
        record("T11 sessions 列表", not err, txt, 0)

        # T12 大输出（base64 EncodedCommand：绕开外层 PS 的引号/$ 展开坑）
        import base64
        ps = '1..5000 | ForEach-Object { "LINE-{0:D4}" -f $_ } | Select-Object -Last 3'
        b64 = base64.b64encode(ps.encode("utf-16-le")).decode()
        run(m, sid, "T12 大输出（EncodedCommand）", f"powershell -NoProfile -EncodedCommand {b64}",
            wait_ms=30000, check=lambda o: "LINE-5000" in o)

        # T13 提示符清理回归：输出里不应再出现 PS 提示符行
        out = run(m, sid, "T13 输出无提示符残留", "echo CLEAN-TEST", wait_ms=8000)
        ok13 = "CLEAN-TEST" in out and "PS C:\\" not in out
        results.append(("T13 输出无提示符残留", ok13, 0, out))
        print(f"[{'PASS' if ok13 else 'FAIL'}] T13 输出无提示符残留 :: {out[:120]}")

    finally:
        try:
            m.tool("rdterm_close", {"session": sid})
        except Exception:
            pass
        m.close()

    # 汇总
    npass = sum(1 for _, ok, _, _ in results if ok)
    with open(REPORT, "w", encoding="utf-8") as f:
        f.write(f"# rdterm MCP 测试报告（对端 {PEER}）\n\n")
        f.write(f"- 时间：{time.strftime('%Y-%m-%d %H:%M:%S')}\n- 结果：{npass}/{len(results)} 通过\n\n")
        f.write("| # | 测试 | 结果 | 耗时s | 备注 |\n|---|---|---|---|---|\n")
        for i, (name, ok, el, detail) in enumerate(results, 1):
            note = detail.replace("\n", "<br>").replace("|", "\\|")[:600]
            f.write(f"| {i} | {name} | {'✅' if ok else '❌'} | {el:.1f} | {note} |\n")
    print(f"\n=== {npass}/{len(results)} PASS, 报告: {REPORT}")


if __name__ == "__main__":
    main()
