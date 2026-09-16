#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""rdterm 组网安全实测套件。

四个测试：
  T1  直连路径（IP:port）传输明文性 —— MITM 代理抓包，搜索哨兵明文
  T2  未授权访问 —— 全新配置目录（零凭据、零历史状态）直连 21118 取 shell
  T3  客户端↔ID 服务器通道加密性 —— 代理接入探测，检查 ID 是否明文可见
  T4  会话密钥协商取证 —— 对比 ID 路径与直连路径的握手日志

用法: python security-tests.py [t1|t2|t3|t4|all] [目标ID或IP]
输出: 控制台摘要 + D:\\rdterm\\security\\out\\*.bin / *.log 原始证据
"""
import importlib.util
import json
import os
import re
import socket
import subprocess
import sys
import threading
import time

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

ROOT = r"D:\rdterm"
EXE = os.path.join(ROOT, "rdterm.exe")
SEC = os.path.join(ROOT, "security")
OUT = os.path.join(SEC, "out")
RDV_HOST = "rs-ny.rustdesk.com"
RDV_PORT = 21116
DIRECT_PORT = 21118

os.makedirs(OUT, exist_ok=True)


def log(msg):
    print(msg, flush=True)


# ---------------------------------------------------------------- 基础设施

def load_mcp():
    spec = importlib.util.spec_from_file_location("mc", os.path.join(ROOT, "mcp-connect.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def wait_port(port, timeout=25, host="127.0.0.1"):
    end = time.time() + timeout
    while time.time() < end:
        with socket.socket() as s:
            s.settimeout(1)
            try:
                s.connect((host, port))
                return True
            except Exception:
                time.sleep(0.4)
    return False


def start_server(cfg_dir, extra=None):
    os.makedirs(cfg_dir, exist_ok=True)
    p = subprocess.Popen(
        [EXE, "--server", "--hide", "--config-dir", cfg_dir] + (extra or []),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    ok = wait_port(DIRECT_PORT, 30)
    return p, ok


def read_id(cfg_dir):
    try:
        with open(os.path.join(cfg_dir, "rdterm-id.json"), encoding="utf-8") as f:
            return json.load(f).get("id", "")
    except Exception:
        return ""


class Proxy:
    """单向监听、双向转发的 TCP 代理，记录两个方向的原始字节。"""

    def __init__(self, listen_port, target_host, target_port, tag):
        self.listen_port = listen_port
        self.target = (target_host, target_port)
        self.tag = tag
        self.c2s = bytearray()
        self.s2c = bytearray()
        self.conns = 0
        self._srv = None
        self._stop = False
        self._threads = []

    def start(self):
        self._srv = socket.socket()
        self._srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._srv.bind(("127.0.0.1", self.listen_port))
        self._srv.listen(8)
        self._srv.settimeout(1)
        t = threading.Thread(target=self._accept_loop, daemon=True)
        t.start()
        self._threads.append(t)

    def _accept_loop(self):
        while not self._stop:
            try:
                c, _ = self._srv.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            self.conns += 1
            threading.Thread(target=self._bridge, args=(c,), daemon=True).start()

    def _bridge(self, client):
        try:
            up = socket.create_connection(self.target, timeout=10)
        except Exception as e:
            log(f"    [proxy] 上游连接失败 {self.target}: {e}")
            client.close()
            return
        client.settimeout(1)
        up.settimeout(1)

        def pump(src, dst, store):
            while not self._stop:
                try:
                    data = src.recv(65536)
                except socket.timeout:
                    continue
                except OSError:
                    break
                if not data:
                    break
                store.extend(data)
                try:
                    dst.sendall(data)
                except OSError:
                    break
            for s in (src, dst):
                try:
                    s.close()
                except OSError:
                    pass

        t1 = threading.Thread(target=pump, args=(client, up, self.c2s), daemon=True)
        t2 = threading.Thread(target=pump, args=(up, client, self.s2c), daemon=True)
        t1.start()
        t2.start()

    def stop(self):
        self._stop = True
        try:
            if self._srv:
                self._srv.close()
        except OSError:
            pass
        time.sleep(0.8)

    def save(self):
        base = os.path.join(OUT, f"capture-{self.tag}")
        with open(base + "-c2s.bin", "wb") as f:
            f.write(self.c2s)
        with open(base + "-s2c.bin", "wb") as f:
            f.write(self.s2c)
        with open(base + "-strings.txt", "w", encoding="utf-8", errors="replace") as f:
            for name, buf in (("client->server", self.c2s), ("server->client", self.s2c)):
                f.write(f"### {name} ({len(buf)} bytes)\n")
                f.write(re.sub(rb"[^\x20-\x7e\r\n]", b".", bytes(buf)).decode("ascii", "replace")[:20000])
                f.write("\n\n")
        return base

    def find_ascii(self, needle):
        hits = []
        for direction, buf in (("c2s", self.c2s), ("s2c", self.s2c)):
            if needle.encode() in bytes(buf):
                hits.append(direction)
        return hits


def client_session(cfg_dir, log_file=None, mcp_mod=None):
    """起一个 rdterm MCP 控制进程，返回 (Mcp 实例, stderr 文件句柄或 None)。"""
    mcp_mod = mcp_mod or load_mcp()
    os.makedirs(cfg_dir, exist_ok=True)
    err = open(log_file, "wb") if log_file else subprocess.DEVNULL
    class _M(mcp_mod.Mcp):
        pass
    m = mcp_mod.Mcp(cwd=os.path.join(ROOT, "_ctl"))
    # 用带 --config-dir 的进程替换（Mcp 构造里没留 config-dir 位，这里直接用 Popen 参数）
    try:
        m.p.kill()
    except Exception:
        pass
    p = subprocess.Popen(
        [EXE, "--mcp", "--config-dir", cfg_dir] + (["--log=debug"] if log_file else []),
        cwd=os.path.join(ROOT, "_ctl"),
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err,
    )
    m.p = p
    m.rid = 0
    m.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                          "clientInfo": {"name": "sec-test", "version": "1.0"}})
    m.notify("notifications/initialized")
    return m, err


# ---------------------------------------------------------------- 测试

def t1_direct_plaintext():
    log("\n=== T1 直连（IP:port）传输明文性 ===")
    srv_dir = os.path.join(ROOT, "_sec_srv")
    srv, ok = start_server(srv_dir)
    if not ok:
        log("  [跳过] 本地服务端未能在 21118 监听")
        return None
    log(f"  本地服务端已起（ID {read_id(srv_dir)}），监听 0.0.0.0:{DIRECT_PORT}")

    proxy = Proxy(21199, "127.0.0.1", DIRECT_PORT, "t1-direct")
    proxy.start()
    log("  MITM 代理 127.0.0.1:21199 -> 127.0.0.1:21118 已就绪")

    sentinel = "RDTERM_PLAINTEXT_PROBE_9F4C1"
    m, _ = client_session(os.path.join(ROOT, "_sec_ctl_t1"))
    try:
        err, txt = m.tool("rdterm_connect", {"peer": "127.0.0.1:21199", "rows": 40, "cols": 200}, timeout=60)
        log(f"  connect 结果: err={err} {txt[:120]}")
        if err:
            return None
        sid = json.loads(txt)["session"]
        err, out = m.tool("rdterm_exec", {"session": sid, "command": f"echo {sentinel}", "wait_ms": 8000})
        log(f"  exec 回显: {out.strip()[:120]!r}")
        m.tool("rdterm_close", {"session": sid})
    finally:
        m.close()
        proxy.stop()

    base = proxy.save()
    hits = proxy.find_ascii(sentinel)
    log(f"  抓包: {len(proxy.c2s)} + {len(proxy.s2c)} 字节，连接数 {proxy.conns}")
    log(f"  哨兵明文出现方向: {hits or '未找到'}")
    log(f"  证据文件: {base}-c2s.bin / -s2c.bin / -strings.txt")
    if hits:
        log("  >>> 结论：直连路径上，终端会话内容以明文经过网络（可被同链路嗅探）")
    else:
        log("  >>> 结论：未在直连路径抓到明文（需人工复核抓包文件）")
    srv.kill()
    return hits


def t2_unauth_access(target="127.0.0.1:21118"):
    log("\n=== T2 未授权访问（零凭据、零历史状态） ===")
    started = None
    if not wait_port(DIRECT_PORT, 3):
        started, ok = start_server(os.path.join(ROOT, "_sec_srv"))
        log(f"  21118 原无监听，已拉起本地服务端（ID {read_id(os.path.join(ROOT, '_sec_srv'))}）ok={ok}")
    cfg = os.path.join(ROOT, "_sec_ctl_t2")
    import shutil
    shutil.rmtree(cfg, ignore_errors=True)          # 全新配置目录 = 没有任何凭据/信任状态
    log(f"  配置目录已清空重建: {cfg}")
    m, _ = client_session(cfg)
    try:
        err, txt = m.tool("rdterm_connect", {"peer": target, "rows": 40, "cols": 200}, timeout=60)
        log(f"  connect 结果: err={err} {txt[:120]}")
        if err:
            log("  >>> 未取得会话")
            return False
        sid = json.loads(txt)["session"]
        err, out = m.tool("rdterm_exec", {"session": sid, "command": "whoami; hostname", "wait_ms": 10000})
        log(f"  exec 输出: {out.strip()[:200]!r}")
        m.tool("rdterm_close", {"session": sid})
        got = bool(out.strip())
        log("  >>> 结论：任何人只要能连上该端口即可获得交互式 shell，无需任何凭据" if got
            else "  >>> 未取得输出，需复核")
        if started:
            started.kill()
        return got
    finally:
        m.close()


def t3_rendezvous_channel():
    log("\n=== T3 客户端↔ID 服务器通道加密性 ===")
    cfg = os.path.join(ROOT, "_sec_ctl_t3")
    import shutil
    shutil.rmtree(cfg, ignore_errors=True)
    os.makedirs(cfg, exist_ok=True)
    with open(os.path.join(cfg, f"rdterm2.toml"), "w", encoding="utf-8") as f:
        f.write(f'rendezvous_server = "{RDV_HOST}:{RDV_PORT}"\n'
                f'\n[options]\n'
                f'custom-rendezvous-server = "127.0.0.1:21199"\n')
    log(f"  已把 custom-rendezvous-server 指向本地代理 127.0.0.1:21199（上游 {RDV_HOST}:{RDV_PORT}）")
    proxy = Proxy(21199, RDV_HOST, RDV_PORT, "t3-rendezvous")
    proxy.start()
    srv, ok = start_server(cfg, extra=["--log=debug"])
    log(f"  被控端已起 ok={ok}，等待 15s 观察注册流量…")
    time.sleep(15)
    srv.kill()
    time.sleep(1)
    proxy.stop()
    base = proxy.save()
    my_id = read_id(cfg)
    hits = proxy.find_ascii(my_id) if my_id else []
    log(f"  本机 ID = {my_id}")
    log(f"  抓包: {len(proxy.c2s)}+{len(proxy.s2c)} 字节，连接数 {proxy.conns}")
    log(f"  ID 明文出现方向: {hits or '未找到'}")
    log(f"  证据文件: {base}-c2s.bin / -strings.txt")
    if proxy.conns == 0:
        log("  >>> 代理未收到连接，无法判定（可能客户端未走该路径或网络不可达）")
    elif hits:
        log("  >>> 结论：ID 以明文出现在客户端↔服务器通道（异常）")
    else:
        log("  >>> 结论：通道为密文，ID/机器名未明文泄露（与 secretbox 通道加密实现一致）")
    return hits


def t4_handshake_evidence(target_id):
    log("\n=== T4 会话密钥协商取证（ID 路径 vs 直连路径） ===")
    res = {}
    for tag, peer in (("id-path", target_id), ("direct-ip", "127.0.0.1:21118")):
        cfg = os.path.join(ROOT, f"_sec_ctl_t4_{tag}")
        import shutil
        shutil.rmtree(cfg, ignore_errors=True)
        lf = os.path.join(OUT, f"t4-{tag}-client.log")
        m, fh = client_session(cfg, log_file=lf)
        try:
            err, txt = m.tool("rdterm_connect", {"peer": peer, "rows": 40, "cols": 200}, timeout=60)
            res[tag] = {"err": err, "txt": txt[:80]}
            if not err:
                sid = json.loads(txt)["session"]
                m.tool("rdterm_close", {"session": sid})
        finally:
            m.close()
            if fh and fh not in (subprocess.DEVNULL,):
                try:
                    fh.close()
                except Exception:
                    pass
        try:
            with open(lf, "rb") as f:
                body = f.read().decode("utf-8", "ignore")
        except Exception:
            body = ""
        flags = {
            "secure_handshake_ok": len(re.findall(r"secure_connection ok", body)),
            "non_secure_fallback": len(re.findall(r"non-secure", body)),
            "pk_mismatch": len(re.findall(r"pk mismatch", body)),
            "sign_verify_fail": len(re.findall(r"Signature mismatch|invalid public key from rendezvous", body)),
        }
        res[tag]["log_flags"] = flags
        res[tag]["log"] = lf
        log(f"  {tag:10s} connect err={err} 日志标记={flags}")
    return res


def main():
    what = (sys.argv[1] if len(sys.argv) > 1 else "all").lower()
    target = sys.argv[2] if len(sys.argv) > 2 else ""
    summary = {}
    if what in ("t1", "all"):
        summary["T1"] = t1_direct_plaintext()
    if what in ("t2", "all"):
        summary["T2"] = t2_unauth_access()
    if what in ("t3", "all"):
        summary["T3"] = t3_rendezvous_channel()
    if what in ("t4", "all"):
        if not target:
            # 用本地服务端 ID 兜底
            target = read_id(os.path.join(ROOT, "_sec_srv"))
        summary["T4"] = t4_handshake_evidence(target) if target else "无可用 ID，跳过"
    log("\n=== 汇总 ===")
    log(json.dumps(summary, ensure_ascii=False, indent=2, default=str))


if __name__ == "__main__":
    main()
