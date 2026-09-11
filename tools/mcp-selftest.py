"""rdterm MCP 一键自测：在本机起一个被控端，再用 MCP 客户端连回去跑几条命令。

用法（在被控端那台机器上跑即可，不需要第二台机器）：
    python mcp-selftest.py            # 连本机 127.0.0.1
    python mcp-selftest.py 192.168.1.20 384849039434

它会：
  1. 在 exe 同目录下建 _selftest_be / _selftest_ctl 两个临时配置目录（跑完删掉），
     免得污染正式配置、也免得两边共用同一个 ID；
  2. 起 `rdterm --server`（开直连监听 21118）；
  3. 起 `rdterm --mcp`（stdio），走完整 JSON-RPC：initialize -> tools/list
     -> rdterm_identity -> rdterm_connect -> rdterm_exec x N -> rdterm_close；
  4. 把两边日志打出来。

注意：所有子进程都在本脚本内托管，脚本一退出它们就结束——这是故意的，
方便在受限环境里跑。要长期驻留请用 rdterm --hide 自己起被控端。
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
PEER = sys.argv[1] if len(sys.argv) > 1 else "127.0.0.1"
WITH_SERVER = PEER in ("127.0.0.1", "localhost")

BE = os.path.join(HERE, "_selftest_be")
CTL = os.path.join(HERE, "_selftest_ctl")


class Mcp:
    def __init__(self, proc):
        self.p = proc
        self.rid = 0

    def send(self, obj):
        self.p.stdin.write((json.dumps(obj, ensure_ascii=False) + "\n").encode("utf-8"))
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
        txt = res.get("content", [{}])[0].get("text", "")
        return res.get("isError", False), txt


def wait_port(port, timeout=20):
    for _ in range(timeout * 2):
        s = socket.socket()
        s.settimeout(1)
        up = s.connect_ex(("127.0.0.1", port)) == 0
        s.close()
        if up:
            return True
        time.sleep(0.5)
    return False


def main():
    if not os.path.exists(EXE):
        sys.exit("找不到 rdterm.exe（应和本脚本放在同一目录）")
    for d in (BE, CTL):
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d, exist_ok=True)

    be = ctl = None
    logs = {}
    try:
        if WITH_SERVER:
            logs["be"] = open(os.path.join(HERE, "_selftest_be.log"), "wb")
            be = subprocess.Popen([EXE, "--config-dir", BE, "--server"],
                                  cwd=BE, stdout=logs["be"], stderr=subprocess.STDOUT)
            print("被控端已启动 pid", be.pid, "，等直连端口 21118 …", flush=True)
            print("  21118:", "已监听" if wait_port(21118) else "未监听（继续尝试）", flush=True)

        logs["ctl"] = open(os.path.join(HERE, "_selftest_ctl.log"), "wb")
        ctl = subprocess.Popen([EXE, "--config-dir", CTL, "--mcp"], cwd=CTL,
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=logs["ctl"])
        m = Mcp(ctl)
        r = m.call("initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "rdterm-selftest", "version": "1"}})
        print("initialize ->", r.get("result", {}).get("serverInfo"))
        m.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

        names = [t["name"] for t in
                 m.call("tools/list", {}).get("result", {}).get("tools", [])]
        print("tools ->", names)

        err, txt = m.tool("rdterm_identity", {})
        print("identity ->", "ERR " if err else "OK", txt)

        t0 = time.time()
        err, txt = m.tool("rdterm_connect", {"peer": PEER, "rows": 24, "cols": 120})
        print(f"connect {PEER} ({time.time()-t0:.1f}s) ->", "ERR " if err else "OK", txt)
        if err:
            return
        sid = json.loads(txt)["session"]

        for cmd in ["echo RDTERM-SELFTEST", "whoami", "hostname", "cd"]:
            t0 = time.time()
            e, out = m.tool("rdterm_exec", {"session": sid, "command": cmd})
            print(f"exec {cmd!r} ({time.time()-t0:.1f}s) ->")
            for line in out.splitlines():
                print("    |", line)

        e, out = m.tool("rdterm_sessions", {})
        print("sessions ->", out)
        print("close ->", m.tool("rdterm_close", {"session": sid})[1])
    finally:
        for p in (be, ctl):
            if p:
                p.terminate()
        for f in logs.values():
            f.close()
        for d in (BE, CTL):
            shutil.rmtree(d, ignore_errors=True)
        for name in ("be", "ctl"):
            fp = os.path.join(HERE, f"_selftest_{name}.log")
            if os.path.exists(fp):
                print(f"===== {name} 日志 =====")
                print(open(fp, "rb").read().decode("utf-8", "ignore")[-1500:])
                os.remove(fp)


if __name__ == "__main__":
    main()
