"""通过 MCP 协议接管远端：起被控端 + MCP stdio 客户端，跑一组真实命令。

这就是 Agent 接管的完整链路（initialize -> tools/list -> connect -> exec*N -> close），
等价于 WorkBuddy / Claude Desktop 里 MCP 连接器做的事，只是这里用脚本直接驱动。
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.path.join(HERE, "rdterm.exe")
BE = os.path.join(HERE, "_be")
CTL = os.path.join(HERE, "_ctl")


class Mcp:
    def __init__(self, p):
        self.p = p
        self.rid = 0

    def send(self, o):
        self.p.stdin.write((json.dumps(o, ensure_ascii=False) + "\n").encode("utf-8"))
        self.p.stdin.flush()

    def call(self, method, params=None):
        self.rid += 1
        self.send({"jsonrpc": "2.0", "id": self.rid, "method": method,
                   "params": params or {}})
        while True:
            line = self.p.stdout.readline()
            if not line:
                return {}
            r = json.loads(line.decode("utf-8", "ignore"))
            if r.get("id") == self.rid:
                return r

    def tool(self, name, args):
        r = self.call("tools/call", {"name": name, "arguments": args})
        res = r.get("result", {})
        return res.get("isError", False), res.get("content", [{}])[0].get("text", "")


def main():
    for d in (BE, CTL):
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d, exist_ok=True)

    be_log = open(os.path.join(HERE, "_be.log"), "wb")
    ctl_log = open(os.path.join(HERE, "_ctl.log"), "wb")
    be = subprocess.Popen([EXE, "--config-dir", BE, "--server"], cwd=BE,
                          stdout=be_log, stderr=subprocess.STDOUT)
    for _ in range(40):
        time.sleep(0.5)
        s = socket.socket()
        s.settimeout(1)
        up = s.connect_ex(("127.0.0.1", 21118)) == 0
        s.close()
        if up:
            break
    print("[1] 被控端已就绪（pid %d，直连端口 21118）\n" % be.pid, flush=True)

    ctl = subprocess.Popen([EXE, "--config-dir", CTL, "--mcp"], cwd=CTL,
                           stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=ctl_log)
    m = Mcp(ctl)
    r = m.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                              "clientInfo": {"name": "agent", "version": "1"}})
    print("[2] MCP 握手 ->", r.get("result", {}).get("serverInfo"))
    m.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    names = [t["name"] for t in m.call("tools/list", {}).get("result", {}).get("tools", [])]
    print("[3] 可用工具 ->", names)
    err, txt = m.tool("rdterm_identity", {})
    print("[4] 本机 ID  ->", txt, "\n")

    err, txt = m.tool("rdterm_connect", {"peer": "127.0.0.1", "rows": 40, "cols": 140})
    print("[5] 连接被控端 ->", "ERR " if err else "OK", txt)
    if err:
        return
    sid = json.loads(txt)["session"]

    cmds = [
        ("whoami", 5000),
        ("hostname", 5000),
        ("[System.Environment]::OSVersion.VersionString", 6000),
        ("ipconfig | Select-String 'IPv4'", 8000),
        ("Get-ChildItem $env:TEMP | Select-Object -First 3 Name", 8000),
        ("Set-Content -Path $env:TEMP\\rdterm-takeover.txt -Value 'MCP 接管写入成功'", 6000),
        ("Get-Content $env:TEMP\\rdterm-takeover.txt", 6000),
        ("Remove-Item $env:TEMP\\rdterm-takeover.txt -Force", 5000),
    ]
    for cmd, wait in cmds:
        t0 = time.time()
        e, out = m.tool("rdterm_exec", {"session": sid, "command": cmd, "wait_ms": wait})
        print(f"$ {cmd}    ({time.time()-t0:.1f}s)")
        if not out.strip():
            print("    (无输出)")
        for line in out.splitlines():
            print("    " + line)
        print(flush=True)

    print("[6] 关闭会话 ->", m.tool("rdterm_close", {"session": sid})[1])
    be.terminate()
    ctl.terminate()
    be_log.close()
    ctl_log.close()
    for d in (BE, CTL):
        shutil.rmtree(d, ignore_errors=True)
    for f in ("_be.log", "_ctl.log"):
        fp = os.path.join(HERE, f)
        if os.path.exists(fp):
            os.remove(fp)


if __name__ == "__main__":
    main()
