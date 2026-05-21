//! BorderCollie CLI（`collie`）
//!
//! 命令组：
//!   collie login / logout / whoami                  身份
//!   collie repo {list,add,rm,toggle}                仓库管理
//!   collie model {list,add,rm,use,show}             模型管理
//!   collie agent {list,toggle,rm}                   智能体管理
//!   collie reviewer list                            审核人员
//!   collie review [--base/--head/--diff-from-stdin] 本地 diff 审查（默认全推断）
//!   collie review-branch --repo NAME --branch BR    让后端拉 diff 审查
//!   collie status / result / list / retry           任务查询
//!   collie upgrade [--check|--run]                  自更新
//!
//! 所有命令统一走 `/api/v1/cli/**`（X-API-Key 鉴权）。

use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use colored::Colorize;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    thread::sleep,
    time::Duration,
};

const DEFAULT_API: &str = "https://review.woofcloud.com";
/// GitHub Release 仓库；可由 BC_RELEASE_REPO env 覆盖，便于 fork 自托管。
const DEFAULT_RELEASE_REPO: &str = "woofcloud/border-collie";
/// 默认 install.sh 来源；最终回落到这里。
const DEFAULT_INSTALL_SH: &str = "https://review.woofcloud.com/install.sh";
const POLL_INTERVAL_SECS: u64 = 3;
const POLL_MAX_ATTEMPTS: u32 = 60;

// ─────────────────────────────────────────────────────────────
// CLI 定义
// ─────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "collie", version, about = "BorderCollie — AI code reviewer CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 浏览器 device-code 登录；落地 `~/.bordercollie/config.toml`。CI 用 --non-interactive --key …
    Login {
        #[arg(long)]
        api: Option<String>,
        /// CI 模式：跳过浏览器，直接落地传入的 X-API-Key
        #[arg(long, requires = "key")]
        non_interactive: bool,
        /// 与 --non-interactive 配合使用
        #[arg(long)]
        key: Option<String>,
    },
    /// 注销：删除本地 config 并调后端吊销 X-API-Key
    Logout,

    /// 写入本地配置：API 地址 + X-API-Key（保留供 CI 与脚本使用；交互场景请用 `collie login`）
    Init {
        #[arg(long)]
        api: Option<String>,
        #[arg(long)]
        key: String,
    },
    /// 显示当前 API Key 绑定的租户、套餐
    Whoami,

    /// 仓库管理
    Repo {
        #[command(subcommand)]
        sub: RepoCmd,
    },
    /// 模型管理
    Model {
        #[command(subcommand)]
        sub: ModelCmd,
    },
    /// Agent / 多角色审查器管理
    Agent {
        #[command(subcommand)]
        sub: AgentCmd,
    },
    /// 审核人员（reviewer）
    Reviewer {
        #[command(subcommand)]
        sub: ReviewerCmd,
    },

    /// 用 `git diff base..head` 提交一次审查；base/head/repo 默认自动推断
    Review {
        /// 默认：upstream → main → master → develop（远端实存的第一个）
        #[arg(long)]
        base: Option<String>,
        /// 默认：当前 HEAD
        #[arg(long)]
        head: Option<String>,
        /// 默认：git remote.origin.url 解析
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        no_wait: bool,
        #[arg(long, conflicts_with_all = ["base", "head"])]
        diff_from_stdin: bool,
    },
    /// 让后端去拉 base..branch 直接跑审查（依赖 repo 已配 access_token）
    #[command(name = "review-branch")]
    ReviewBranch {
        /// repo 名（必须先 `collie repo add` 或在控制台配好）
        #[arg(long)]
        repo: String,
        /// 要审查的分支
        #[arg(long)]
        branch: String,
        /// 等待完成
        #[arg(long)]
        no_wait: bool,
    },
    /// 查询任务简洁状态
    Status { id: i64 },
    /// 查询任务详细结果（含 review_summary / findings）
    Result { id: i64 },
    /// 列出最近的任务
    List {
        #[arg(long, default_value_t = 1)]
        page: u64,
        #[arg(long, default_value_t = 10)]
        size: u64,
        #[arg(long)]
        status: Option<String>,
    },
    /// 重试一个失败的任务
    Retry { id: i64 },

    /// 查看当前租户的配额 + 用量
    Usage,
    /// 列出对外公开的套餐目录（来自后端 tenant_id=0 模板）
    Plans,
    /// 查看模型调用日志（可按 task_id 过滤）
    Logs {
        #[arg(long)]
        task_id: Option<i64>,
        #[arg(long, default_value_t = 1)]
        page: u64,
        #[arg(long, default_value_t = 10)]
        size: u64,
        #[arg(long)]
        status: Option<String>,
    },
    /// 显示本地 ~/.bordercollie/config.toml
    Config,
    /// 生成 shell 补全脚本：`collie completion bash > ~/.bordercollie-completion.bash`
    Completion {
        /// bash / zsh / fish / powershell / elvish
        shell: Shell,
    },

    /// 检查最新版本（默认）；带 --run 则直接调用 install.sh 走一遍升级流程
    Upgrade {
        /// 仅检查不下载（默认行为；保留作为显式参数）
        #[arg(long)]
        check: bool,
        /// 直接运行 install.sh 完成升级（前提：你的安装是通过 install.sh 完成的）
        #[arg(long, conflicts_with = "check")]
        run: bool,
        /// 自定义 install.sh URL（默认从已有 BC_API 推断；最终回落 review.woofcloud.com）
        #[arg(long)]
        install_url: Option<String>,
    },
}

#[derive(Subcommand)]
enum RepoCmd {
    /// 列当前租户的仓库
    List,
    /// 新建 / 更新仓库（必填 --name、--url、--platform、--token）
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        url: String,
        /// github / gitlab / gitee / cnb
        #[arg(long)]
        platform: String,
        /// 访问该仓库的 token（会被加密落库）
        #[arg(long)]
        token: String,
        #[arg(long)]
        webhook_secret: Option<String>,
        /// 默认绑定的模型 id（可选）
        #[arg(long)]
        model_id: Option<i64>,
    },
    /// 删除仓库（按 id）
    Rm { id: i64 },
    /// 切换仓库启用状态
    Toggle { id: i64 },
}

#[derive(Subcommand)]
enum ModelCmd {
    List,
    /// 注册一个新模型
    Add {
        #[arg(long)]
        name: String,
        /// openai / anthropic / deepseek / qwen / ollama / custom
        #[arg(long)]
        provider: String,
        #[arg(long)]
        api_url: String,
        #[arg(long)]
        api_key: String,
        #[arg(long, default_value_t = 4096)]
        max_tokens: i32,
        #[arg(long, default_value_t = 0.7)]
        temperature: f64,
    },
    Rm { id: i64 },
    /// 设为默认模型
    Use { id: i64 },
    /// 看单个模型详情
    Show { id: i64 },
}

#[derive(Subcommand)]
enum AgentCmd {
    List,
    /// 新建一个 Agent
    Add {
        #[arg(long)]
        name: String,
        /// safety / performance / style / sql / general 等
        #[arg(long)]
        role_type: String,
        /// system prompt（也可用 --system-prompt-file）
        #[arg(long)]
        system_prompt: Option<String>,
        /// 从文件读取 system prompt（与 --system-prompt 二选一）
        #[arg(long)]
        system_prompt_file: Option<String>,
        /// 关联模型 id，多个用逗号分隔
        #[arg(long)]
        model_ids: Option<String>,
        /// 执行顺序（越小越先）
        #[arg(long, default_value_t = 0)]
        execution_order: i32,
        /// 权重（默认 100）
        #[arg(long, default_value_t = 100)]
        weight: i32,
    },
    Rm { id: i64 },
    Toggle { id: i64 },
}

#[derive(Subcommand)]
enum ReviewerCmd {
    List,
    /// 新建审核人员
    Add {
        #[arg(long)]
        name: String,
        /// general / security / performance / style / database 等
        #[arg(long)]
        category: String,
        /// 关联的关键词或语言标签，逗号分隔
        #[arg(long)]
        language_tags: Option<String>,
        #[arg(long)]
        system_prompt: Option<String>,
        #[arg(long)]
        system_prompt_file: Option<String>,
        /// 绑定模型 id
        #[arg(long)]
        model_id: Option<i64>,
    },
    Rm { id: i64 },
}

// ─────────────────────────────────────────────────────────────
// 配置 + HTTP
// ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct Profile {
    api: String,
    key: String,
}

fn config_path() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("无法定位 home 目录"))?
        .join(".bordercollie");
    fs::create_dir_all(&dir).with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
    Ok(dir.join("config.toml"))
}

fn load_profile() -> Result<Profile> {
    let path = config_path()?;
    if !path.exists() {
        bail!(
            "未找到配置：{}\n请先运行 `collie login` 或 `collie init --api <url> --key <X-API-Key>`",
            path.display()
        );
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("读取配置失败: {}", path.display()))?;
    let mut p: Profile = toml::from_str(&raw).context("配置文件不是合法 TOML")?;
    if p.api.is_empty() {
        p.api = DEFAULT_API.to_string();
    }
    p.api = p.api.trim_end_matches('/').to_string();
    if p.key.is_empty() {
        bail!("配置缺少 key 字段，请重新 `collie login` 或 `collie init --key ...`");
    }
    Ok(p)
}

fn save_profile(p: &Profile) -> Result<()> {
    let path = config_path()?;
    fs::write(&path, toml::to_string_pretty(p).context("序列化配置失败")?)
        .with_context(|| format!("写入配置失败: {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    println!("{} {}", "✓ 配置已写入".green(), path.display());
    Ok(())
}

/// 删除本地配置（best-effort）
fn delete_profile() -> Result<()> {
    let path = config_path()?;
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("删除配置失败: {}", path.display()))?;
        println!("{} {}", "✓ 已删除".green(), path.display());
    }
    Ok(())
}

fn client() -> Client {
    Client::builder()
        .user_agent(format!("bordercollie-cli/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build reqwest client")
}

#[derive(Deserialize, Debug)]
struct Envelope<T> {
    code: i64,
    msg: Option<String>,
    data: Option<T>,
}

fn req(profile: &Profile, method: Method, path: &str) -> RequestBuilder {
    let url = format!("{}{}", profile.api, path);
    client().request(method, &url).header("X-API-Key", &profile.key)
}

fn call_json<T: serde::de::DeserializeOwned>(rb: RequestBuilder) -> Result<T> {
    let resp = rb.send().context("HTTP 请求失败")?;
    let status = resp.status();
    let body = resp.text().context("读取响应体失败")?;
    if !status.is_success() {
        bail!("HTTP {}: {}", status.as_u16(), body);
    }
    let env: Envelope<T> =
        serde_json::from_str(&body).with_context(|| format!("响应不是合法 JSON: {}", body))?;
    if env.code != 10000000 {
        bail!("接口失败 code={}: {}", env.code, env.msg.unwrap_or_default());
    }
    env.data
        .ok_or_else(|| anyhow!("响应缺少 data 字段：{}", body))
}

fn call_unit(rb: RequestBuilder) -> Result<()> {
    let resp = rb.send().context("HTTP 请求失败")?;
    let status = resp.status();
    let body = resp.text().context("读取响应体失败")?;
    if !status.is_success() {
        bail!("HTTP {}: {}", status.as_u16(), body);
    }
    let env: Envelope<Value> =
        serde_json::from_str(&body).with_context(|| format!("响应不是合法 JSON: {}", body))?;
    if env.code != 10000000 {
        bail!("接口失败 code={}: {}", env.code, env.msg.unwrap_or_default());
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────
// git 辅助
// ─────────────────────────────────────────────────────────────

fn run_git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("执行 git {} 失败（系统没装 git？）", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} 返回非零：\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 自动推断 base：
/// 1. 当前分支的 upstream（@{upstream}）
/// 2. 远端 origin 上实存的第一个 main / master / develop
fn infer_base() -> Option<String> {
    if let Ok(up) = run_git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"]) {
        if !up.is_empty() {
            return Some(up);
        }
    }
    for cand in ["origin/main", "origin/master", "origin/develop", "main", "master", "develop"] {
        if run_git(&["rev-parse", "--verify", "--quiet", cand]).is_ok() {
            return Some(cand.to_string());
        }
    }
    None
}

/// HEAD 默认就是 HEAD（与 git 习惯一致）
fn infer_head() -> String {
    "HEAD".to_string()
}

fn infer_repo_name() -> String {
    if let Ok(url) = run_git(&["config", "--get", "remote.origin.url"]) {
        if let Some(idx) = url.rfind(':') {
            let tail = &url[idx + 1..];
            let trimmed = tail.trim_end_matches(".git").to_string();
            if !trimmed.is_empty() && !trimmed.contains("//") {
                return trimmed;
            }
        }
        let path_part = url.trim_end_matches(".git");
        let mut segments: Vec<&str> = path_part.split('/').collect();
        if segments.len() >= 2 {
            let name = segments.pop().unwrap_or("");
            let owner = segments.pop().unwrap_or("");
            if !owner.is_empty() && !name.is_empty() {
                return format!("{}/{}", owner, name);
            }
        }
    }
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "local".to_string())
}

fn read_stdin() -> Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("读取 stdin 失败")?;
    Ok(buf)
}

// ─────────────────────────────────────────────────────────────
// 响应模型
// ─────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct WhoAmI {
    tenant_id: i64,
    username: String,
    plan: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct RepoItem {
    id: Option<i64>,
    repo_name: Option<String>,
    platform: Option<String>,
    repo_url: Option<String>,
    review_enabled: Option<i8>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct ModelItem {
    id: Option<i64>,
    model_name: Option<String>,
    provider: Option<String>,
    api_url: Option<String>,
    is_default: Option<i8>,
    is_enabled: Option<i8>,
    max_tokens: Option<i32>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct AgentItem {
    id: Option<i64>,
    name: Option<String>,
    role_type: Option<String>,
    execution_order: Option<i32>,
    enabled: Option<i8>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct ReviewerItem {
    id: Option<i64>,
    name: Option<String>,
    category: Option<String>,
    language_tags: Option<String>,
    is_builtin: Option<i8>,
    enabled: Option<i8>,
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct StatusData {
    task_id: i64,
    task_no: Option<String>,
    status: Option<String>,
    risk_level: Option<String>,
    score: Option<i32>,
    review_summary: Option<String>,
    error_msg: Option<String>,
    created_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct ResultData {
    task_id: i64,
    task_no: Option<String>,
    repo_name: Option<String>,
    source_branch: Option<String>,
    target_branch: Option<String>,
    status: Option<String>,
    risk_level: Option<String>,
    score: Option<i32>,
    gate_status: Option<String>,
    author: Option<String>,
    commit_sha: Option<String>,
    additions: Option<i32>,
    deletions: Option<i32>,
    review_summary: Option<String>,
    review_detail: Option<String>,
    secret_findings: Option<String>,
    license_findings: Option<String>,
    sast_findings: Option<String>,
    error_msg: Option<String>,
    created_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct Page<T> {
    records: Vec<T>,
    total: i64,
    size: i64,
    current: i64,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct TaskRow {
    id: Option<i64>,
    task_no: Option<String>,
    repo_name: Option<String>,
    status: Option<String>,
    risk_level: Option<String>,
    score: Option<i32>,
    created_at: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct SubmitData {
    task_id: i64,
    task_no: String,
    status: String,
    #[allow(dead_code)]
    message: String,
}

// ───── device-code OAuth DTO（与后端 review_cli_device_code 实体对齐） ─────

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct DeviceCodeResp {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    interval: i32,
    expires_in: i32,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct PollResp {
    status: String,
    api_key: Option<String>,
    tenant_id: Option<i64>,
    #[allow(dead_code)]
    username: Option<String>,
    plan: Option<String>,
}

// ─────────────────────────────────────────────────────────────
// 打印工具
// ─────────────────────────────────────────────────────────────

fn yn(v: Option<i8>) -> &'static str {
    match v.unwrap_or(0) {
        1 => "yes",
        _ => "no",
    }
}

fn status_color(s: &str) -> colored::ColoredString {
    match s {
        "success" | "completed" | "passed" => s.green(),
        "failed" | "error" => s.red(),
        "blocked" => s.red().bold(),
        "running" | "queued" | "pending" => s.yellow(),
        _ => s.normal(),
    }
}

fn risk_color(s: &str) -> colored::ColoredString {
    match s {
        "critical" => s.red().bold(),
        "high" => s.red(),
        "medium" => s.yellow(),
        "low" => s.green(),
        _ => s.normal(),
    }
}

fn print_status(s: &StatusData) {
    println!();
    println!(
        "{} #{} {}",
        "Task".bold(),
        s.task_id,
        s.task_no.as_deref().unwrap_or("").dimmed()
    );
    let st = s.status.as_deref().unwrap_or("?");
    println!("  status     : {}", status_color(st));
    if let Some(r) = &s.risk_level {
        println!("  risk       : {}", risk_color(r));
    }
    if let Some(score) = s.score {
        println!("  score      : {}", score);
    }
    if let Some(t) = &s.created_at {
        println!("  created at : {}", t);
    }
    if let Some(t) = &s.completed_at {
        println!("  finished at: {}", t);
    }
    if let Some(err) = &s.error_msg {
        if !err.is_empty() {
            println!("\n{}\n{}", "× error".red().bold(), err);
        }
    }
    if let Some(sum) = &s.review_summary {
        if !sum.is_empty() {
            println!("\n{}\n{}", "▸ summary".bold(), sum);
        }
    }
    println!();
}

// ─────────────────────────────────────────────────────────────
// 命令实现
// ─────────────────────────────────────────────────────────────

/// `collie login` —— 浏览器 device-code（RFC 8628）或 CI 非交互模式
fn cmd_login(api: Option<String>, non_interactive: bool, key: Option<String>) -> Result<()> {
    let api_url = api
        .or_else(|| std::env::var("BC_API").ok())
        .unwrap_or_else(|| DEFAULT_API.to_string());
    let api_url = api_url.trim_end_matches('/').to_string();

    if non_interactive {
        let key = key.ok_or_else(|| anyhow!("--non-interactive 需要配合 --key"))?;
        return save_profile(&Profile {
            api: api_url,
            key,
        });
    }

    // 1. 申请 device_code + user_code
    let host = hostname_or_unknown();
    let body = serde_json::json!({ "clientMeta": format!("collie-cli {} on {}", env!("CARGO_PKG_VERSION"), host) });
    let url = format!("{}/api/v1/cli/device/code", api_url);
    let resp = client()
        .post(&url)
        .json(&body)
        .send()
        .context("申请 device code 失败")?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().unwrap_or_default();
        bail!("HTTP {}: {}", s.as_u16(), t);
    }
    let env: Envelope<DeviceCodeResp> = serde_json::from_str(&resp.text()?)?;
    if env.code != 10000000 {
        bail!(
            "申请 device code 失败 code={}: {}",
            env.code,
            env.msg.unwrap_or_default()
        );
    }
    let dc = env
        .data
        .ok_or_else(|| anyhow!("响应缺少 data"))?;

    // 2. 提示用户
    println!();
    println!("{}", "▸ open the URL in your browser and confirm the code:".bold());
    println!("    {}", dc.verification_uri.cyan());
    println!("    {} {}", "code:".dimmed(), dc.user_code.green().bold());
    println!();
    println!(
        "  {} {}",
        "(or open the prefilled URL:".dimmed(),
        format!("{})", dc.verification_uri_complete).dimmed()
    );
    println!();

    // 3. 轮询，直到 approved / expired / denied
    let interval = dc.interval.max(1) as u64;
    let max_attempts = ((dc.expires_in as u64) / interval).max(1);
    let poll_url = format!("{}/api/v1/cli/device/poll", api_url);
    let poll_body = serde_json::json!({ "deviceCode": dc.device_code });
    print!("  {} ", "waiting…".dimmed());
    use std::io::Write;
    std::io::stdout().flush().ok();

    for _ in 0..max_attempts {
        sleep(Duration::from_secs(interval));
        print!(".");
        std::io::stdout().flush().ok();
        let resp = client().post(&poll_url).json(&poll_body).send();
        let resp = match resp {
            Ok(r) => r,
            Err(_) => continue, // 网络抖动，重试
        };
        let text = resp.text().unwrap_or_default();
        let env: Envelope<PollResp> = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if env.code != 10000000 {
            continue;
        }
        let p = match env.data {
            Some(v) => v,
            None => continue,
        };
        match p.status.as_str() {
            "approved" => {
                let key = p
                    .api_key
                    .ok_or_else(|| anyhow!("approved 但 API key 缺失，服务端可能已清理；请重试 collie login"))?;
                println!();
                save_profile(&Profile {
                    api: api_url.clone(),
                    key,
                })?;
                println!(
                    "{} tenant={} plan={}",
                    "✓ signed in".green(),
                    p.tenant_id.unwrap_or(0),
                    p.plan.as_deref().unwrap_or("free")
                );
                return Ok(());
            }
            "denied" => {
                println!();
                bail!("用户在浏览器侧拒绝了授权");
            }
            "expired" => {
                println!();
                bail!("device code 已过期，请重试 collie login");
            }
            _ => continue, // pending
        }
    }
    println!();
    bail!("等待超时；可重试 `collie login`")
}

fn cmd_logout() -> Result<()> {
    if let Ok(p) = load_profile() {
        // best-effort：吊销当前 X-API-Key
        let _ = req(&p, Method::DELETE, "/api/v1/cli/device/token")
            .send()
            .map(|r| r.error_for_status().ok());
    }
    delete_profile()?;
    Ok(())
}

fn hostname_or_unknown() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| {
            run_git(&["config", "--get", "user.email"]).unwrap_or_else(|_| "unknown".to_string())
        })
}

fn cmd_whoami() -> Result<()> {
    let p = load_profile()?;
    let me: WhoAmI = call_json(req(&p, Method::GET, "/api/v1/cli/whoami"))?;
    println!("{}", "▸ identity".bold());
    println!("  api        : {}", p.api.dimmed());
    println!("  tenant id  : {}", me.tenant_id);
    println!("  api key    : {}", me.username);
    println!("  plan       : {}", me.plan.as_deref().unwrap_or("free"));
    Ok(())
}

fn cmd_repo_list() -> Result<()> {
    let p = load_profile()?;
    let list: Vec<RepoItem> = call_json(req(&p, Method::GET, "/api/v1/cli/repo/list"))?;
    if list.is_empty() {
        println!("{}", "(no repos)".dimmed());
        return Ok(());
    }
    println!("{:>5}  {:<28}  {:<10}  {:<6}  {}", "ID", "REPO", "PLATFORM", "ON", "URL");
    for r in list {
        println!(
            "{:>5}  {:<28}  {:<10}  {:<6}  {}",
            r.id.unwrap_or(0),
            r.repo_name.as_deref().unwrap_or("?"),
            r.platform.as_deref().unwrap_or("?"),
            yn(r.review_enabled),
            r.repo_url.as_deref().unwrap_or("").dimmed(),
        );
    }
    Ok(())
}

fn cmd_repo_add(
    name: String,
    url: String,
    platform: String,
    token: String,
    webhook_secret: Option<String>,
    model_id: Option<i64>,
) -> Result<()> {
    let p = load_profile()?;
    let body = serde_json::json!({
        "repoName": name,
        "platform": platform,
        "repoUrl": url,
        "accessToken": token,
        "webhookSecret": webhook_secret,
        "reviewEnabled": 1,
        "aiReviewEnabled": 1,
        "defaultModelId": model_id,
    });
    call_unit(req(&p, Method::POST, "/api/v1/cli/repo").json(&body))?;
    println!("{} repo {} saved", "✓".green(), name);
    Ok(())
}

fn cmd_repo_rm(id: i64) -> Result<()> {
    let p = load_profile()?;
    call_unit(req(&p, Method::DELETE, &format!("/api/v1/cli/repo/{}", id)))?;
    println!("{} repo #{} deleted", "✓".green(), id);
    Ok(())
}

fn cmd_repo_toggle(id: i64) -> Result<()> {
    let p = load_profile()?;
    call_unit(req(&p, Method::PUT, &format!("/api/v1/cli/repo/{}/toggle", id)))?;
    println!("{} repo #{} toggled", "✓".green(), id);
    Ok(())
}

fn cmd_model_list() -> Result<()> {
    let p = load_profile()?;
    let list: Vec<ModelItem> = call_json(req(&p, Method::GET, "/api/v1/cli/model/list"))?;
    if list.is_empty() {
        println!("{}", "(no models)".dimmed());
        return Ok(());
    }
    println!(
        "{:>5}  {:<24}  {:<14}  {:<7}  {:<6}  {}",
        "ID", "NAME", "PROVIDER", "DEFAULT", "ON", "API"
    );
    for m in list {
        println!(
            "{:>5}  {:<24}  {:<14}  {:<7}  {:<6}  {}",
            m.id.unwrap_or(0),
            m.model_name.as_deref().unwrap_or("?"),
            m.provider.as_deref().unwrap_or("?"),
            yn(m.is_default),
            yn(m.is_enabled),
            m.api_url.as_deref().unwrap_or("").dimmed(),
        );
    }
    Ok(())
}

fn cmd_model_add(
    name: String,
    provider: String,
    api_url: String,
    api_key: String,
    max_tokens: i32,
    temperature: f64,
) -> Result<()> {
    let p = load_profile()?;
    let body = serde_json::json!({
        "modelName": name,
        "provider": provider,
        "apiUrl": api_url,
        "apiKey": api_key,
        "maxTokens": max_tokens,
        "temperature": temperature,
        "isEnabled": 1,
    });
    call_unit(req(&p, Method::POST, "/api/v1/cli/model").json(&body))?;
    println!("{} model {} saved", "✓".green(), name);
    Ok(())
}

fn cmd_model_rm(id: i64) -> Result<()> {
    let p = load_profile()?;
    call_unit(req(&p, Method::DELETE, &format!("/api/v1/cli/model/{}", id)))?;
    println!("{} model #{} deleted", "✓".green(), id);
    Ok(())
}

fn cmd_model_use(id: i64) -> Result<()> {
    let p = load_profile()?;
    call_unit(req(&p, Method::PUT, &format!("/api/v1/cli/model/{}/default", id)))?;
    println!("{} model #{} set as default", "✓".green(), id);
    Ok(())
}

fn cmd_model_show(id: i64) -> Result<()> {
    let p = load_profile()?;
    let list: Vec<ModelItem> = call_json(req(&p, Method::GET, "/api/v1/cli/model/list"))?;
    let m = list
        .into_iter()
        .find(|m| m.id == Some(id))
        .ok_or_else(|| anyhow!("model {} 不存在", id))?;
    println!("{}", "▸ model".bold());
    println!("  id       : {}", m.id.unwrap_or(0));
    println!("  name     : {}", m.model_name.as_deref().unwrap_or("-"));
    println!("  provider : {}", m.provider.as_deref().unwrap_or("-"));
    println!("  api url  : {}", m.api_url.as_deref().unwrap_or("-"));
    println!("  default  : {}", yn(m.is_default));
    println!("  enabled  : {}", yn(m.is_enabled));
    println!("  max toks : {}", m.max_tokens.unwrap_or(0));
    Ok(())
}

fn cmd_agent_list() -> Result<()> {
    let p = load_profile()?;
    let list: Vec<AgentItem> = call_json(req(&p, Method::GET, "/api/v1/cli/agent/list"))?;
    if list.is_empty() {
        println!("{}", "(no agents)".dimmed());
        return Ok(());
    }
    println!("{:>5}  {:<28}  {:<14}  {:<5}  {}", "ID", "NAME", "ROLE", "ORDER", "ON");
    for a in list {
        println!(
            "{:>5}  {:<28}  {:<14}  {:<5}  {}",
            a.id.unwrap_or(0),
            a.name.as_deref().unwrap_or("?"),
            a.role_type.as_deref().unwrap_or("?"),
            a.execution_order.unwrap_or(0),
            yn(a.enabled),
        );
    }
    Ok(())
}

fn cmd_agent_add(
    name: String,
    role_type: String,
    system_prompt: Option<String>,
    system_prompt_file: Option<String>,
    model_ids: Option<String>,
    execution_order: i32,
    weight: i32,
) -> Result<()> {
    let p = load_profile()?;
    let prompt = read_prompt(system_prompt, system_prompt_file)?
        .ok_or_else(|| anyhow!("必须提供 --system-prompt 或 --system-prompt-file"))?;
    let body = serde_json::json!({
        "name": name,
        "roleType": role_type,
        "systemPrompt": prompt,
        "modelIds": model_ids,
        "executionOrder": execution_order,
        "weight": weight,
        "enabled": 1,
    });
    let id: i64 = call_json(req(&p, Method::POST, "/api/v1/cli/agent").json(&body))?;
    println!("{} agent created: id={}", "✓".green(), id);
    Ok(())
}

fn cmd_agent_rm(id: i64) -> Result<()> {
    let p = load_profile()?;
    call_unit(req(&p, Method::DELETE, &format!("/api/v1/cli/agent/{}", id)))?;
    println!("{} agent #{} deleted", "✓".green(), id);
    Ok(())
}

fn cmd_agent_toggle(id: i64) -> Result<()> {
    let p = load_profile()?;
    call_unit(req(&p, Method::PUT, &format!("/api/v1/cli/agent/{}/toggle", id)))?;
    println!("{} agent #{} toggled", "✓".green(), id);
    Ok(())
}

fn cmd_reviewer_list() -> Result<()> {
    let p = load_profile()?;
    let list: Vec<ReviewerItem> = call_json(req(&p, Method::GET, "/api/v1/cli/reviewer/list"))?;
    if list.is_empty() {
        println!("{}", "(no reviewers)".dimmed());
        return Ok(());
    }
    println!(
        "{:>5}  {:<28}  {:<14}  {:<7}  {:<6}  {}",
        "ID", "NAME", "CATEGORY", "BUILTIN", "ON", "LANGUAGES"
    );
    for r in list {
        println!(
            "{:>5}  {:<28}  {:<14}  {:<7}  {:<6}  {}",
            r.id.unwrap_or(0),
            r.name.as_deref().unwrap_or("?"),
            r.category.as_deref().unwrap_or("-"),
            yn(r.is_builtin),
            yn(r.enabled),
            r.language_tags.as_deref().unwrap_or("").dimmed(),
        );
    }
    Ok(())
}

fn cmd_reviewer_add(
    name: String,
    category: String,
    language_tags: Option<String>,
    system_prompt: Option<String>,
    system_prompt_file: Option<String>,
    model_id: Option<i64>,
) -> Result<()> {
    let p = load_profile()?;
    let prompt = read_prompt(system_prompt, system_prompt_file)?;
    let body = serde_json::json!({
        "name": name,
        "category": category,
        "languageTags": language_tags,
        "systemPromptOverride": prompt,
        "modelId": model_id,
        "enabled": 1,
    });
    call_unit(req(&p, Method::POST, "/api/v1/cli/reviewer").json(&body))?;
    println!("{} reviewer {} saved", "✓".green(), name);
    Ok(())
}

fn cmd_reviewer_rm(id: i64) -> Result<()> {
    let p = load_profile()?;
    call_unit(req(&p, Method::DELETE, &format!("/api/v1/cli/reviewer/{}", id)))?;
    println!("{} reviewer #{} deleted", "✓".green(), id);
    Ok(())
}

fn read_prompt(inline: Option<String>, file: Option<String>) -> Result<Option<String>> {
    match (inline, file) {
        (Some(s), _) => Ok(Some(s)),
        (None, Some(path)) => Ok(Some(
            fs::read_to_string(&path).with_context(|| format!("读取 {} 失败", path))?,
        )),
        (None, None) => Ok(None),
    }
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct UsageSummary {
    plan_name: Option<String>,
    plan_display: Option<String>,
    status: Option<String>,
    limits: PlanLimits,
    current: CurrentUsage,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct PlanLimits {
    max_repos: Option<i64>,
    max_reviews_per_day: Option<i64>,
    max_reviews_per_month: Option<i64>,
    max_tokens_per_month: Option<i64>,
    max_team_members: Option<i64>,
    max_concurrent_reviews: Option<i64>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct CurrentUsage {
    today_reviews: i64,
    today_tokens: i64,
    month_reviews: i64,
    month_tokens: i64,
    repo_count: i64,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct PlanItem {
    plan_name: Option<String>,
    plan_display: Option<String>,
    price_cny: Option<f64>,
    price_usd: Option<f64>,
    billing_period: Option<String>,
    max_repos: Option<i64>,
    max_reviews_per_day: Option<i64>,
    max_tokens_per_month: Option<i64>,
    is_popular: Option<i8>,
    description: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct ModelLogRow {
    id: Option<i64>,
    task_id: Option<i64>,
    model_name: Option<String>,
    provider: Option<String>,
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
    total_tokens: Option<i32>,
    duration_ms: Option<i64>,
    status: Option<String>,
    created_at: Option<String>,
}

fn cmd_usage() -> Result<()> {
    let p = load_profile()?;
    let u: UsageSummary = call_json(req(&p, Method::GET, "/api/v1/cli/usage"))?;
    println!("{}", "▸ plan".bold());
    println!(
        "  {} {} {}",
        u.plan_name.as_deref().unwrap_or("?"),
        format!("({})", u.plan_display.as_deref().unwrap_or("-")).dimmed(),
        u.status.as_deref().unwrap_or("-")
    );

    fn pct(cur: i64, limit: Option<i64>) -> String {
        match limit {
            Some(l) if l > 0 => {
                let p = (cur as f64 / l as f64) * 100.0;
                let bar = bar_of(p);
                let line = format!("{:>10} / {:<10}  {}  {:>5.1}%", cur, l, bar, p);
                if p >= 90.0 { line.red().to_string() }
                else if p >= 70.0 { line.yellow().to_string() }
                else { line }
            }
            _ => format!("{:>10} / {:<10}", cur, "∞"),
        }
    }
    println!("\n{}", "▸ usage".bold());
    println!("  repos        {}", pct(u.current.repo_count, u.limits.max_repos));
    println!("  reviews/day  {}", pct(u.current.today_reviews, u.limits.max_reviews_per_day));
    println!("  reviews/mo   {}", pct(u.current.month_reviews, u.limits.max_reviews_per_month));
    println!("  tokens/mo    {}", pct(u.current.month_tokens, u.limits.max_tokens_per_month));
    if let Some(seats) = u.limits.max_team_members {
        println!("  team seats   {:>10} / {:<10}", "?", seats);
    }
    if let Some(c) = u.limits.max_concurrent_reviews {
        println!("  concurrent   {:>10} / {:<10}", "—", c);
    }
    Ok(())
}

fn bar_of(percent: f64) -> String {
    let width = 20.0;
    let filled = ((percent / 100.0) * width).clamp(0.0, width) as usize;
    let empty = (width as usize) - filled;
    format!("[{}{}]", "█".repeat(filled), "·".repeat(empty))
}

fn cmd_plans() -> Result<()> {
    let p = load_profile()?;
    let list: Vec<PlanItem> = call_json(req(&p, Method::GET, "/api/v1/cli/plans"))?;
    if list.is_empty() {
        println!("{}", "(no plans)".dimmed());
        return Ok(());
    }
    println!(
        "{:<14}  {:<14}  {:<10}  {:<10}  {:<12}  {}",
        "NAME", "DISPLAY", "PRICE", "REPOS", "TOKENS/MO", "DESC"
    );
    for plan in list {
        let price = match plan.billing_period.as_deref() {
            Some("free") => "free".to_string(),
            _ => format!("¥{:.0}/mo", plan.price_cny.unwrap_or(0.0)),
        };
        let popular = if plan.is_popular == Some(1) { " ★".yellow().to_string() } else { "".to_string() };
        println!(
            "{:<14}  {:<14}  {:<10}  {:<10}  {:<12}  {}{}",
            plan.plan_name.as_deref().unwrap_or("-"),
            plan.plan_display.as_deref().unwrap_or("-"),
            price,
            plan.max_repos.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string()),
            plan.max_tokens_per_month.map(|v| format!("{}k", v / 1000)).unwrap_or_else(|| "-".to_string()),
            plan.description.as_deref().unwrap_or("").dimmed(),
            popular,
        );
    }
    Ok(())
}

fn cmd_logs(task_id: Option<i64>, page: u64, size: u64, status: Option<String>) -> Result<()> {
    let p = load_profile()?;
    let mut path = format!("/api/v1/cli/logs/page?current={}&size={}", page, size);
    if let Some(t) = task_id {
        use std::fmt::Write;
        let _ = write!(path, "&taskId={}", t);
    }
    if let Some(s) = status {
        use std::fmt::Write;
        let _ = write!(path, "&status={}", urlencoding::encode(&s));
    }
    let data: Page<ModelLogRow> = call_json(req(&p, Method::GET, &path))?;
    if data.records.is_empty() {
        println!("{}", "(no model logs)".dimmed());
        return Ok(());
    }
    println!(
        "{:>6}  {:>7}  {:<18}  {:<10}  {:>6}  {:>6}  {:>7}  {:<8}  {}",
        "ID", "TASK", "MODEL", "PROVIDER", "PROMPT", "COMPL", "TOTAL", "STATUS", "WHEN"
    );
    for row in data.records {
        println!(
            "{:>6}  {:>7}  {:<18}  {:<10}  {:>6}  {:>6}  {:>7}  {:<8}  {}",
            row.id.unwrap_or(0),
            row.task_id.unwrap_or(0),
            row.model_name.as_deref().unwrap_or("-"),
            row.provider.as_deref().unwrap_or("-"),
            row.prompt_tokens.unwrap_or(0),
            row.completion_tokens.unwrap_or(0),
            row.total_tokens.unwrap_or(0),
            status_color(row.status.as_deref().unwrap_or("-")),
            row.created_at.as_deref().unwrap_or("-").dimmed(),
        );
    }
    println!(
        "\n{}",
        format!(
            "page {}/{} · total {}",
            data.current,
            (data.total + data.size - 1).max(1) / data.size.max(1),
            data.total
        )
        .dimmed()
    );
    Ok(())
}

fn cmd_config() -> Result<()> {
    let path = config_path()?;
    if !path.exists() {
        println!("{} {}", "config not found:".yellow(), path.display());
        println!("  run `collie login` or `collie init --api <url> --key <X-API-Key>` first");
        return Ok(());
    }
    let raw = fs::read_to_string(&path)?;
    println!("{} {}", "▸".bold(), path.display());
    for line in raw.lines() {
        let mask = if line.trim_start().starts_with("key") {
            let parts: Vec<&str> = line.splitn(2, '=').collect();
            if parts.len() == 2 {
                let v = parts[1].trim().trim_matches('"');
                let masked = if v.len() > 8 {
                    format!("{}***{}", &v[..4], &v[v.len() - 4..])
                } else {
                    "***".to_string()
                };
                format!("{} = \"{}\"", parts[0].trim_end(), masked)
            } else {
                line.to_string()
            }
        } else {
            line.to_string()
        };
        println!("  {}", mask);
    }
    Ok(())
}

fn cmd_completion(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "collie", &mut std::io::stdout());
    Ok(())
}

#[derive(Deserialize, Debug, Default)]
struct GhRelease {
    tag_name: Option<String>,
    html_url: Option<String>,
}

fn cmd_upgrade(check: bool, run: bool, install_url: Option<String>) -> Result<()> {
    let _ = check; // 显式 --check 与默认行为一致，只用作可读性
    let current = env!("CARGO_PKG_VERSION");
    let repo = std::env::var("BC_RELEASE_REPO")
        .unwrap_or_else(|_| DEFAULT_RELEASE_REPO.to_string());

    println!("{} {}", "▸ current".bold(), current.cyan());
    println!("  {} {}", "release repo".dimmed(), repo);

    // GitHub API 不需要 X-API-Key，单独构造一次性 client
    let api = format!("https://api.github.com/repos/{}/releases/latest", repo);
    let resp = client()
        .get(&api)
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("查询 GitHub Release 失败")?;
    if !resp.status().is_success() {
        bail!(
            "GitHub HTTP {} — 仓库 {} 可能不存在或还没发布过 release",
            resp.status().as_u16(),
            repo
        );
    }
    let rel: GhRelease = resp.json().context("解析 GitHub 响应失败")?;
    let tag = rel
        .tag_name
        .as_deref()
        .ok_or_else(|| anyhow!("GitHub 返回里缺少 tag_name"))?;
    // 同时兼容 `cli-vX.Y.Z`（我们的发布约定）与 `vX.Y.Z`（很多仓库的通用约定）
    let latest_num = tag
        .trim_start_matches("cli-v")
        .trim_start_matches('v')
        .to_string();

    println!("{} {}", "▸ latest".bold(), latest_num.cyan());
    if let Some(u) = &rel.html_url {
        println!("  {} {}", "release page".dimmed(), u);
    }

    if version_at_least(&latest_num, current) && latest_num != current {
        println!(
            "\n{} {}\n",
            "✓ 有新版本可升级。".green(),
            format!("v{}", latest_num).bold()
        );
        let url = install_url
            .or_else(|| std::env::var("BC_INSTALL_URL").ok())
            .unwrap_or_else(|| DEFAULT_INSTALL_SH.to_string());

        if run {
            println!("{} {}", "▸ running".bold(), &url);
            let status = Command::new("sh")
                .arg("-c")
                .arg(format!("curl -fsSL '{}' | sh", url))
                .status()
                .context("无法启动 sh，尝试手动运行：curl -fsSL ... | sh")?;
            if !status.success() {
                bail!("install.sh 退出码非零 ({:?})", status.code());
            }
        } else {
            println!("{}", "  一键升级：".bold());
            println!("    curl -fsSL {} | sh", url);
            println!("  {}", "或：collie upgrade --run（脚本会探测 OS/arch 并替换 ~/.local/bin/collie）".dimmed());
        }
    } else {
        println!("\n{} 已是最新版本。", "✓".green());
    }
    Ok(())
}

/// 简化版语义比较：把 SemVer 字符串按 `.` 拆成数字逐位比较；非数字段当 0。
/// 仅在 `a > b` 时返回 true。预发布后缀（-rc.x）不参与比较。
fn version_at_least(a: &str, b: &str) -> bool {
    fn parts(s: &str) -> Vec<u32> {
        s.split('-')
            .next()
            .unwrap_or(s)
            .split('.')
            .map(|p| p.parse::<u32>().unwrap_or(0))
            .collect()
    }
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let av = pa.get(i).copied().unwrap_or(0);
        let bv = pb.get(i).copied().unwrap_or(0);
        if av != bv {
            return av > bv;
        }
    }
    false
}

fn cmd_review(
    base: Option<String>,
    head: Option<String>,
    repo: Option<String>,
    no_wait: bool,
    diff_from_stdin: bool,
) -> Result<()> {
    let p = load_profile()?;
    let (base, head, diff) = if diff_from_stdin {
        let d = read_stdin()?;
        if d.trim().is_empty() {
            bail!("stdin 没有读到 diff");
        }
        // stdin 模式下 base/head 仅用于元数据
        (
            base.unwrap_or_else(|| "stdin".to_string()),
            head.unwrap_or_else(|| "stdin".to_string()),
            d,
        )
    } else {
        let base = base
            .or_else(infer_base)
            .ok_or_else(|| anyhow!("无法推断 base 分支，请显式 --base"))?;
        let head = head.unwrap_or_else(infer_head);
        let d = run_git(&["diff", &format!("{}..{}", base, head)])?;
        if d.trim().is_empty() {
            bail!(
                "`git diff {}..{}` 没有差异——如果你确实想审查无差异的状态，可以 --diff-from-stdin",
                base,
                head
            );
        }
        (base, head, d)
    };
    let repo_name = repo
        .or_else(|| std::env::var("BC_REPO").ok())
        .unwrap_or_else(infer_repo_name);
    let author = run_git(&["config", "--get", "user.email"])
        .or_else(|_| run_git(&["config", "--get", "user.name"]))
        .unwrap_or_else(|_| "cli".to_string());
    let commit_sha = run_git(&["rev-parse", &head]).unwrap_or_default();

    let payload = serde_json::json!({
        "repoName": repo_name,
        "diffContent": diff,
        "sourceBranch": head,
        "targetBranch": base,
        "author": author,
        "commitSha": commit_sha,
        "priority": 0,
        "taskType": "cli",
    });

    println!(
        "{} {} → {} ({})",
        "▸ submit".bold(),
        base.cyan(),
        head.cyan(),
        repo_name.dimmed()
    );

    let s: SubmitData = call_json(req(&p, Method::POST, "/api/v1/cli/review/submit").json(&payload))?;
    println!(
        "{} taskId={} taskNo={} status={}",
        "✓ accepted".green(),
        s.task_id,
        s.task_no,
        s.status
    );

    if no_wait {
        println!("  {}", format!("collie status {}", s.task_id).dimmed());
        return Ok(());
    }
    wait_and_print(&p, s.task_id)
}

fn cmd_review_branch(repo: String, branch: String, no_wait: bool) -> Result<()> {
    let p = load_profile()?;
    // 通过仓库名拿到 repo_id
    let list: Vec<RepoItem> = call_json(req(&p, Method::GET, "/api/v1/cli/repo/list"))?;
    let repo_id = list
        .iter()
        .find(|r| r.repo_name.as_deref() == Some(repo.as_str()))
        .and_then(|r| r.id)
        .ok_or_else(|| {
            anyhow!(
                "找不到名为 {} 的仓库；可用：{}",
                repo,
                list.iter().filter_map(|r| r.repo_name.as_deref()).collect::<Vec<_>>().join(", ")
            )
        })?;

    let payload = serde_json::json!({
        "repoId": repo_id,
        "branch": branch,
        "taskType": "MANUAL",
    });
    println!(
        "{} repo={} branch={} (server-side fetch)",
        "▸ submit".bold(),
        repo.cyan(),
        branch.cyan()
    );
    let resp: Value = call_json(req(&p, Method::POST, "/api/v1/cli/review/branch").json(&payload))?;
    let task_id = resp.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
    let task_no = resp.get("taskNo").and_then(|v| v.as_str()).unwrap_or("");
    println!("{} taskId={} taskNo={}", "✓ accepted".green(), task_id, task_no);
    if no_wait {
        return Ok(());
    }
    wait_and_print(&p, task_id)
}

fn wait_and_print(p: &Profile, task_id: i64) -> Result<()> {
    println!("\n{}", "▸ polling for completion …".bold());
    let mut last = String::new();
    for attempt in 1..=POLL_MAX_ATTEMPTS {
        sleep(Duration::from_secs(POLL_INTERVAL_SECS));
        let s: StatusData =
            call_json(req(p, Method::GET, &format!("/api/v1/cli/review/status/{}", task_id)))?;
        let cur = s.status.clone().unwrap_or_default();
        if cur != last {
            println!("  [{:>2}] status = {}", attempt, status_color(&cur));
            last = cur.clone();
        }
        if matches!(
            cur.as_str(),
            "success" | "completed" | "passed" | "failed" | "error" | "blocked"
        ) {
            print_status(&s);
            if matches!(cur.as_str(), "failed" | "error" | "blocked") {
                std::process::exit(2);
            }
            return Ok(());
        }
    }
    bail!(
        "等待超时：{} 秒后任务仍未完成，可手动 `collie status {}`",
        POLL_INTERVAL_SECS * POLL_MAX_ATTEMPTS as u64,
        task_id
    );
}

fn cmd_status(id: i64) -> Result<()> {
    let p = load_profile()?;
    let s: StatusData = call_json(req(&p, Method::GET, &format!("/api/v1/cli/review/status/{}", id)))?;
    print_status(&s);
    Ok(())
}

fn cmd_result(id: i64) -> Result<()> {
    let p = load_profile()?;
    let r: ResultData = call_json(req(&p, Method::GET, &format!("/api/v1/cli/review/result/{}", id)))?;
    println!();
    println!("{} #{} {}", "Task".bold(), r.task_id, r.task_no.as_deref().unwrap_or("").dimmed());
    println!("  repo       : {}", r.repo_name.as_deref().unwrap_or("-"));
    if r.source_branch.is_some() || r.target_branch.is_some() {
        println!(
            "  branches   : {} → {}",
            r.target_branch.as_deref().unwrap_or("?"),
            r.source_branch.as_deref().unwrap_or("?")
        );
    }
    if let Some(commit) = &r.commit_sha {
        println!("  commit     : {}", commit.dimmed());
    }
    if let Some(author) = &r.author {
        println!("  author     : {}", author);
    }
    println!("  status     : {}", status_color(r.status.as_deref().unwrap_or("?")));
    if let Some(g) = &r.gate_status {
        println!("  gate       : {}", g);
    }
    if let Some(rl) = &r.risk_level {
        println!("  risk       : {}", risk_color(rl));
    }
    if let Some(score) = r.score {
        println!("  score      : {}", score);
    }
    if r.additions.is_some() || r.deletions.is_some() {
        println!(
            "  changes    : {} {} / {} {}",
            "+".green(),
            r.additions.unwrap_or(0),
            "-".red(),
            r.deletions.unwrap_or(0)
        );
    }
    println!("  created at : {}", r.created_at.as_deref().unwrap_or("-"));
    println!("  finished at: {}", r.completed_at.as_deref().unwrap_or("-"));
    if let Some(err) = r.error_msg.as_deref().filter(|s| !s.is_empty()) {
        println!("\n{}\n{}", "× error".red().bold(), err);
    }
    if let Some(sum) = r.review_summary.as_deref().filter(|s| !s.is_empty()) {
        println!("\n{}\n{}", "▸ summary".bold(), sum);
    }
    for (label, val) in [
        ("secret findings", r.secret_findings),
        ("license findings", r.license_findings),
        ("SAST findings", r.sast_findings),
    ] {
        if let Some(v) = val.as_deref().filter(|s| !s.is_empty() && *s != "[]" && *s != "{}") {
            println!("\n{}\n{}", format!("▸ {}", label).bold(), v);
        }
    }
    if let Some(detail) = r.review_detail.as_deref().filter(|s| !s.is_empty()) {
        println!("\n{}\n{}", "▸ detail".bold(), detail);
    }
    println!();
    Ok(())
}

fn cmd_list(page: u64, size: u64, status: Option<String>) -> Result<()> {
    let p = load_profile()?;
    let mut path = format!("/api/v1/cli/review/list?current={}&size={}", page, size);
    if let Some(s) = status {
        use std::fmt::Write;
        let _ = write!(path, "&status={}", urlencoding::encode(&s));
    }
    let page_data: Page<TaskRow> = call_json(req(&p, Method::GET, &path))?;
    if page_data.records.is_empty() {
        println!("{}", "(no tasks)".dimmed());
        return Ok(());
    }
    println!(
        "{:>7}  {:<22}  {:<24}  {:<10}  {:<8}  {:<5}  {}",
        "ID", "TASK_NO", "REPO", "STATUS", "RISK", "SCORE", "CREATED"
    );
    for row in &page_data.records {
        let st = row.status.as_deref().unwrap_or("-");
        let rl = row.risk_level.as_deref().unwrap_or("-");
        println!(
            "{:>7}  {:<22}  {:<24}  {:<10}  {:<8}  {:<5}  {}",
            row.id.unwrap_or(0),
            row.task_no.as_deref().unwrap_or("-"),
            row.repo_name.as_deref().unwrap_or("-"),
            status_color(st),
            risk_color(rl),
            row.score.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string()),
            row.created_at.as_deref().unwrap_or("-").dimmed(),
        );
    }
    println!(
        "\n{}",
        format!(
            "page {}/{} · total {}",
            page_data.current,
            (page_data.total + page_data.size - 1).max(1) / page_data.size.max(1),
            page_data.total
        )
        .dimmed()
    );
    Ok(())
}

fn cmd_retry(id: i64) -> Result<()> {
    let p = load_profile()?;
    call_unit(req(&p, Method::POST, &format!("/api/v1/cli/review/{}/retry", id)))?;
    println!("{} task #{} re-queued", "✓".green(), id);
    Ok(())
}

// 简单 URL encode（避免引外部 crate）；仅处理空格与少数特殊字符
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(c),
                _ => {
                    let mut buf = [0u8; 4];
                    for b in c.encode_utf8(&mut buf).as_bytes() {
                        out.push_str(&format!("%{:02X}", b));
                    }
                }
            }
        }
        out
    }
}

fn main() {
    let cli = Cli::parse();
    let res = match cli.cmd {
        Cmd::Init { api, key } => save_profile(&Profile {
            api: api.unwrap_or_else(|| DEFAULT_API.to_string()),
            key,
        }),
        Cmd::Whoami => cmd_whoami(),
        Cmd::Repo { sub } => match sub {
            RepoCmd::List => cmd_repo_list(),
            RepoCmd::Add { name, url, platform, token, webhook_secret, model_id } => {
                cmd_repo_add(name, url, platform, token, webhook_secret, model_id)
            }
            RepoCmd::Rm { id } => cmd_repo_rm(id),
            RepoCmd::Toggle { id } => cmd_repo_toggle(id),
        },
        Cmd::Model { sub } => match sub {
            ModelCmd::List => cmd_model_list(),
            ModelCmd::Add { name, provider, api_url, api_key, max_tokens, temperature } => {
                cmd_model_add(name, provider, api_url, api_key, max_tokens, temperature)
            }
            ModelCmd::Rm { id } => cmd_model_rm(id),
            ModelCmd::Use { id } => cmd_model_use(id),
            ModelCmd::Show { id } => cmd_model_show(id),
        },
        Cmd::Agent { sub } => match sub {
            AgentCmd::List => cmd_agent_list(),
            AgentCmd::Add {
                name,
                role_type,
                system_prompt,
                system_prompt_file,
                model_ids,
                execution_order,
                weight,
            } => cmd_agent_add(
                name,
                role_type,
                system_prompt,
                system_prompt_file,
                model_ids,
                execution_order,
                weight,
            ),
            AgentCmd::Rm { id } => cmd_agent_rm(id),
            AgentCmd::Toggle { id } => cmd_agent_toggle(id),
        },
        Cmd::Reviewer { sub } => match sub {
            ReviewerCmd::List => cmd_reviewer_list(),
            ReviewerCmd::Add {
                name,
                category,
                language_tags,
                system_prompt,
                system_prompt_file,
                model_id,
            } => cmd_reviewer_add(name, category, language_tags, system_prompt, system_prompt_file, model_id),
            ReviewerCmd::Rm { id } => cmd_reviewer_rm(id),
        },
        Cmd::Login { api, non_interactive, key } => cmd_login(api, non_interactive, key),
        Cmd::Logout => cmd_logout(),
        Cmd::Review { base, head, repo, no_wait, diff_from_stdin } => {
            cmd_review(base, head, repo, no_wait, diff_from_stdin)
        }
        Cmd::ReviewBranch { repo, branch, no_wait } => cmd_review_branch(repo, branch, no_wait),
        Cmd::Status { id } => cmd_status(id),
        Cmd::Result { id } => cmd_result(id),
        Cmd::List { page, size, status } => cmd_list(page, size, status),
        Cmd::Retry { id } => cmd_retry(id),
        Cmd::Usage => cmd_usage(),
        Cmd::Plans => cmd_plans(),
        Cmd::Logs { task_id, page, size, status } => cmd_logs(task_id, page, size, status),
        Cmd::Config => cmd_config(),
        Cmd::Completion { shell } => cmd_completion(shell),
        Cmd::Upgrade { check, run, install_url } => cmd_upgrade(check, run, install_url),
    };
    if let Err(err) = res {
        eprintln!("{} {:#}", "✗".red().bold(), err);
        std::process::exit(1);
    }
}
