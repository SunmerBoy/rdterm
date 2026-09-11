# -*- coding: utf-8 -*-
"""
远程 PowerShell 执行器：把本地 .ps1 base64 编码后在目标机的 PowerShell 会话里执行。

用法:
  python remote-ps.py <ID或IP> <script.ps1> [wait秒]

说明:
  - 脚本用 UTF-16LE base64 下发（-EncodedCommand），绕开引号/中文/换行问题。
  - 输出等待 wait 秒（默认 120）；未跑完会自动轮询 rdterm_read 直到静默。
"""
import base64
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
EXE = os.path.join(ROOT, "rdterm.exe")
CTL = os.path.join(ROOT, "_ctl")


class Mcp:
    def __init__(self):
        os.makedirs(CTL, exist_ok=True)
        self.p = subprocess.Popen(
            [EXE, "--mcp"], cwd=CTL,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        self.rid = 0

    def _send(self, obj):
        self.p.stdin.write((json.dumps(obj, ensure_ascii=False) + "\n").encode("utf-8"))
        self.p.stdin.flush()

    def call(self, method, params=None, timeout=300):
        self.rid += 1
        self._send({"jsonrpc": "2.0", "id": self.rid, "method": method, "params": params or {}})
        while True:
            line = self.p.stdout.readline()
            if not line:
                return {}
            try:
                r = json.loads(line.decode("utf-8", "ignore"))
            except Exception:
                continue
            if r.get("id") == self.rid:
                return r

    def notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def tool(self, name, args=None):
        r = self.call("tools/call", {"name": name, "arguments": args or {}})
        res = r.get("result", {})
        text = "".join(c.get("text", "") for c in res.get("content", []))
        if not text and "error" in r:
            text = json.dumps(r["error"], ensure_ascii=False)
        return res.get("isError", True), text

    def close(self):
        try:
            self.p.terminate()
        except Exception:
            pass


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 1
    peer, ps1 = sys.argv[1], sys.argv[2]
    wait = int(sys.argv[3]) if len(sys.argv) > 3 else 120

    with open(ps1, encoding="utf-8-sig") as f:
        code = f.read()
    b64 = base64.b64encode(code.encode("utf-16-le")).decode("ascii")
    cmd = "powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand " + b64
    if len(cmd) > 30000:
        print("[!] 命令过长: %d 字符" % len(cmd))
        return 1

    m = Mcp()
    try:
        r = m.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                  "clientInfo": {"name": "remote-ps", "version": "1.0"}})
        m.notify("notifications/initialized")
        err, txt = m.tool("rdterm_connect", {"peer": peer, "rows": 50, "cols": 220})
        print("[mcp] connect ->", "ERR " if err else "OK ", txt)
        if err:
            return 2
        sid = json.loads(txt)["session"]

        t0 = time.time()
        err, out = m.tool("rdterm_exec", {"session": sid, "command": cmd, "wait_ms": wait * 1000})
        print(out, end="")
        # 脚本没跑完就继续轮询，直到连续 3 秒无新输出
        idle = 0.0
        while not err:
            time.sleep(1.0)
            e2, more = m.tool("rdterm_read", {"session": sid})
            if e2 or not more.strip():
                idle += 1.0
                if idle >= 3:
                    break
                continue
            idle = 0.0
            print(more, end="", flush=True)
        print("\n[用时 %.1fs]" % (time.time() - t0))
        m.tool("rdterm_close", {"session": sid})
    finally:
        m.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
