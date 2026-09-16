# 安全策略 / Security Policy

rdterm 是 RustDesk 的 **terminal-only** 二次开发版本：**无密码、ID 即凭证、默认开放局域网直连**。这组取舍是为了「Agent 直接接管远端 shell」的便利性，代价是应用层不再提供纵深防御。请先阅读下面的边界说明再部署。

## 部署红线

1. **不要把 `21118`（直连端口）暴露到公网。** 该端口不校验任何凭据，网络可达即等于完全控制。
2. **不要在不可信网络（公共 Wi-Fi、共享交换域、可被镜像的链路）用 IP 直连。** 直连路径的终端会话是**明文**传输的；跨不可信网络请用 ID（rendezvous）建链或点对点 VPN。
3. **ID 视为高敏凭证。** 拿到 ID 即可接管该主机；泄露后请立即重启被控端（会换新 ID）或等待 12 小时自动轮换。
4. **MCP 的 HTTP 传输无鉴权**，只在本机回环使用；跨机请用 stdio。

## 完整分析报告

完整的威胁模型、密码学实现审查、可复现的实测证据（MITM 抓包、零凭据接入、ID 随机性抽样）与风险清单见：

- [`docs/security/rdterm-security-analysis.md`](docs/security/rdterm-security-analysis.md)

配套验证脚本：`tools/security/security-tests.py`、`tools/security/id-analysis.py`

## 上报安全问题

发现漏洞请通过以下任一方式联系，请勿直接公开 issue 细节：

- GitHub 私密漏洞上报：仓库 **Security → Report a vulnerability**
- 邮件：1444840707@qq.com

请附上：受影响版本/提交、复现步骤、影响面评估，以及是否已在真实网络验证。我们会在确认后于报告中更新致谢与修复状态。

## 支持的版本

仅最新 Release 版本接受安全修复；本项目为 fork 快照，不保证与上游 RustDesk 的安全修复同步，重要变更请关注 Release 说明。
