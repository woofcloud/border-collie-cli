# README 截图

把对应的 PNG 文件直接放到这里，仓库根 `README.md` 会自动引用：

| 文件名 | 来源 | 用途 |
|---|---|---|
| `site-hero.png` | `review.woofcloud.com` 首页 hero 一屏（建议 ≥ 1600 宽，2x retina 截图） | README 顶部官网展示 |
| `console-device-approve.png` | `review.woofcloud.com/cli/device`（带 `?code=BDRWC-G34` 预填验证码） | "登录"章节，展示 device-code 授权页 |
| `console-dashboard.png` | `review.woofcloud.com/admin/#/codeReview/dashboard` | "控制台还能看什么"第一张 |
| `console-task-report.png` | `review.woofcloud.com/admin/#/codeReview/task/<某个完整任务>` | "控制台还能看什么"第二张 |
| `console-agent.png` | `review.woofcloud.com/admin/#/codeReview/agent` | "控制台还能看什么"第三张 |

## 截图建议

- **dark theme** 截，跟官网首页色调一致
- **1440×900** 或更大；浏览器开发者工具 device toolbar 选 "Responsive" + 1440 宽更省事
- 不要带浏览器地址栏（用 mac 的 ⌘⇧4 区域截图就行），README 里 markdown 自带描述文字够了
- 文件 < 500 KB；超出可以走 `pngquant` / `oxipng` 压一下

放好后 push 一次，GitHub 渲染的 README 直接显示。
