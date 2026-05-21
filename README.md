# collie — BorderCollie CLI

[![CI](https://github.com/woofcloud/border-collie/actions/workflows/cli-release.yml/badge.svg)](https://github.com/woofcloud/border-collie/actions/workflows/cli-release.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

把分支 / PR 的 diff 提交给 [BorderCollie](https://review.woofcloud.com) 跑一次 AI 评审，命令行版。

```bash
$ curl -fsSL https://review.woofcloud.com/install.sh | sh
$ collie login           # 浏览器 device-code 授权
$ collie review          # 自动推断 base / head / repo
```

BorderCollie 是一只 24/7 在线的 AI code reviewer，从 PR / commit 一推上来就开始读 diff，标 severity、给 patch、超阈值就拦掉 merge。控制台在 <https://review.woofcloud.com/admin>；本仓库只放 CLI 与发布脚手架，CLI 的协议见下文。

> 命令名是 `collie`（不要跟 Unix 自带的 `bc` 计算器混了）；安装产物的 crate / binary 名也都是 `collie`。

---

## 安装

### 一行脚本（推荐）

```bash
curl -fsSL https://review.woofcloud.com/install.sh | sh
```

行为：探测 OS / arch → 拉对应 release artifact + sha256 校验 → 落到 `~/.local/bin/collie` → chmod +x → 自检。可调环境变量：

| 变量 | 默认 | 作用 |
|---|---|---|
| `BC_INSTALL_DIR` | `~/.local/bin` | 安装目录 |
| `BC_VERSION` | (latest) | 指定历史版本，例如 `cli-v2.0.0` |
| `BC_RELEASE_REPO` | `woofcloud/border-collie` | release 源仓（fork 自托管用） |

### Windows

到 [Releases](https://github.com/woofcloud/border-collie/releases/latest) 下载 `collie-x86_64-pc-windows-msvc.zip`，解压后把 `collie.exe` 放进 PATH。

### 从源码

```bash
git clone https://github.com/woofcloud/border-collie.git
cd border-collie
cargo install --path .
```

需要 Rust 1.74+；安装出来的二进制叫 `collie`。

---

## 一分钟上手

```bash
# 1. 安装
curl -fsSL https://review.woofcloud.com/install.sh | sh

# 2. 登录（浏览器 device-code，无需手贴 API Key）
collie login
#   collie 会给你一个 URL 和 8 位 user_code；在浏览器里登录控制台
#   后输入 user_code、点「授权 collie CLI」，CLI 立刻拿到 X-API-Key

# 3. 进任一 git 仓库、跑评审
cd ~/projects/my-app
collie review
#   自动推断：
#     base = 当前分支上游 / origin/main / master / develop（依次找到的第一个）
#     head = HEAD
#     repo = git remote.origin.url 解析
```

## CI 集成（GitHub Action 示例）

```yaml
- uses: actions/checkout@v4
  with: { fetch-depth: 0 }
- run: curl -fsSL https://review.woofcloud.com/install.sh | BC_INSTALL_DIR=$RUNNER_TEMP/bin sh
- run: $RUNNER_TEMP/bin/collie login --non-interactive --key ${{ secrets.BC_API_KEY }}
- run: $RUNNER_TEMP/bin/collie review --base origin/${{ github.base_ref }}
```

退出码：`0` 通过 · `2` blocked / failed → pipeline 自动失败。
想把报告贴到 PR 评论，加 `--format md --out review.md` 再 upload-artifact 即可。

---

## 命令一览

| 类别       | 命令                                                                          |
|------------|-------------------------------------------------------------------------------|
| 身份       | `collie login` / `collie logout` / `collie whoami`                            |
| 本地评审   | `collie review [--base ... --head ... --repo ...]`                            |
| 任务       | `collie status <id>` / `collie result <id>` / `collie list` / `collie retry <id>` |
| 服务端拉取 | `collie review-branch --repo <name> --branch <br>`                            |
| 仓库       | `collie repo {list,add,rm,toggle}`                                            |
| 模型       | `collie model {list,add,rm,use,show}`                                         |
| Agent      | `collie agent {list,add,rm,toggle}`                                           |
| 审核员     | `collie reviewer {list,add,rm}`                                               |
| 配额       | `collie usage` / `collie plans` / `collie logs`                               |
| 升级       | `collie upgrade [--check\|--run]`                                             |
| 杂项       | `collie config` / `collie completion <shell>` / `collie init`（CI 别名）       |

`collie --help` 看全部；`collie <subcommand> --help` 看每个子命令的具体参数。

---

## 配置

落地在 `~/.bordercollie/config.toml`（macOS / Linux 自动 `chmod 600`）：

```toml
api = "https://review.woofcloud.com"
key = "bc_…"
```

环境变量优先级高于 config：`BC_API` / `BC_API_KEY` / `BC_REPO`。

## 默认值推断（`collie review`）

| 字段 | 顺序 |
|------|------|
| `repo` | `--repo` → `BC_REPO` → `git remote.origin.url` 解析 → 当前目录名 |
| `base` | `--base` → 当前分支 upstream → origin/main → origin/master → origin/develop |
| `head` | `--head` → `HEAD` |

`--diff-from-stdin` 时不依赖 git，直接把 stdin 当 diff（CI 友好）。

## 升级

```bash
collie upgrade              # 查看 latest（不下载）
collie upgrade --run        # 调 install.sh 替换当前二进制
```

环境变量：`BC_RELEASE_REPO`（默认 `woofcloud/border-collie`）、`BC_INSTALL_URL`（默认 `https://review.woofcloud.com/install.sh`）。

## 后端约定（API 协议）

CLI 与服务端通信走 `X-API-Key` header，端点全部在 `/api/v1/cli/**`：

| 端点 | 用途 |
|---|---|
| `POST /api/v1/cli/device/code` | `collie login`：申请 device + user code |
| `POST /api/v1/cli/device/poll` | `collie login`：轮询授权状态 |
| `DELETE /api/v1/cli/device/token` | `collie logout`：自吊销当前 API Key |
| `GET  /api/v1/cli/whoami` | `collie whoami` |
| `POST /api/v1/cli/review/submit` | `collie review` 提交本地 diff |
| `POST /api/v1/cli/review/branch` | `collie review-branch` 让服务端拉 diff |
| `GET  /api/v1/cli/review/{status,result,list}` | 任务查询 |
| `POST /api/v1/cli/review/:id/retry` | `collie retry` |
| `/api/v1/cli/{repo,model,agent,reviewer}/...` | 管理类 |
| `GET  /api/v1/cli/{usage,plans,logs/page}` | 配额 + 套餐 + 日志 |

device-code OAuth 的批准端点不在 `/cli/**` 下：`POST /api/v1/review/cli-device/approve`（走 JWT，由控制台 `/cli/device` 页调用）。

---

## 开发 / 构建

```bash
cargo build               # debug
cargo build --release     # 优化产物在 target/release/collie
cargo test                # 内含 device-code 工具方法 unit test
cargo fmt && cargo clippy
```

## 发布

打 `cli-v*` tag 即触发 [cli-release.yml](.github/workflows/cli-release.yml)：

```bash
# bump Cargo.toml::version 后
git tag cli-v2.0.1
git push origin cli-v2.0.1
```

GHA 矩阵跑 5 个 target（linux/macos × x86_64+arm64，windows x86_64），自动出 release notes、产物 + sha256 上 GitHub Release。`review.woofcloud.com/install.sh` 默认拉 latest。

## 已知约束

- macOS / Linux 一脚本安装；Windows 暂走手动下载 + 加 PATH
- 浏览器 device-code 要求服务端 ≥ 2.0；老服务端用 `collie login --non-interactive --key …`
- `collie review --apply` 交互式 patch 勾选在 2.0.x 后续版本中提供

## License

[Apache-2.0](LICENSE)
