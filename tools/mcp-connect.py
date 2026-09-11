#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""
rdterm MCP 接管客户端。

用法:
  python mcp-connect.py <ID或IP>                 # 连上后进入交互式命令输入
  python mcp-connect.py <ID或IP> "cmd1" "cmd2"   # 执行完给定命令后退出

被控端机器上需先运行 rdterm.exe（双击选 1，或 rdterm --server）。
"""
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
EXE = os.path.join(ROOT, "rdterm.exe")
CTL = os.path.join(ROOT, "_ctl")


class Mcp:
    def __init__(self, exe=EXE, cwd=CTL, extra_args=None):
        os.makedirs(cwd, exist_ok=True)
        self.p = subprocess.Popen(
            [exe, "--mcp"] + (extra_args or []),
            cwd=cwd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.rid = 0

    def _send(self, obj):
        self.p.stdin.write((json.dumps(obj, ensure_ascii=False) + "\n").encode("utf-8"))
        self.p.stdin.flush()

    def call(self, method, params=None, timeout=180):
        self.rid += 1
        self._send({"jsonrpc": "2.0", "id": self.rid, "method": method, "params": params or {}})
        # 用线程读太麻烦，这里用阻塞 readline + 外层总超时
        deadline = time.time() + timeout
        while True:
            line = self.p.stdout.readline()
            if not line:
                return {}
            if time.time() > deadline:
                return {"error": "timeout"}
            try:
                r = json.loads(line.decode("utf-8", "ignore"))
            except Exception:
                continue
            if r.get("id") == self.rid:
                return r

    def notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def tool(self, name, args=None, timeout=180):
        r = self.call("tools/call", {"name": name, "arguments": args or {}}, timeout)
        res = r.get("result", {})
        err = res.get("isError", True)
        text = ""
        for c in res.get("content", []):
            text += c.get("text", "")
        if not text and "error" in r:
            err, text = True, json.dumps(r["error"], ensure_ascii=False)
        return err, text

    def close(self):
        try:
            self.p.terminate()
        except Exception:
            pass


def main():
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        return 1
    peer = args[0]
    cmds = args[1:]

    m = Mcp()
    try:
        r = m.call("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "mcp-connect", "version": "1.0"},
        })
        m.notify("notifications/initialized")
        info = r.get("result", {}).get("serverInfo", {})
        print(f"[mcp] 已连接 MCP 服务: {info.get('name','?')} {info.get('version','')}")

        t0 = time.time()
        err, txt = m.tool("rdterm_connect", {"peer": peer, "rows": 40, "cols": 200}, timeout=90)
        print(f"[mcp] rdterm_connect({peer}) {time.time()-t0:.1f}s ->", "ERR " if err else "OK ", txt)
        if err:
            return 2
        sid = json.loads(txt)["session"]

        if cmds:
            for c in cmds:
                t0 = time.time()
                e, out = m.tool("rdterm_exec", {"session": sid, "command": c, "max_wait_ms": 30000})
                print(f"\n$ {c}   ({time.time()-t0:.1f}s)")
                print(out if out.strip() else ("<无输出>" if not e else out))
            m.tool("rdterm_close", {"session": sid})
            return 0

        print("\n已进入远程终端，输入命令回车执行；exit/quit 退出。")
        while True:
            try:
                c = input("remote> ").strip()
            except (EOFError, KeyboardInterrupt):
                print()
                break
            if not c:
                continue
            if c.lower() in ("exit", "quit"):
                break
            e, out = m.tool("rdterm_exec", {"session": sid, "command": c, "max_wait_ms": 30000})
            print(out)
        m.tool("rdterm_close", {"session": sid})
    finally:
        m.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
