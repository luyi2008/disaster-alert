# CLAUDE.md

给 Claude Code 的仓库须知。完整规范见 [CONTRIBUTING.md](CONTRIBUTING.md),这里只列 Claude 每次改动前必须遵守的要点。

## 分支命名

新分支从 `main` 切出,格式为 `<类型>/<简短描述>`,类型限定为 `feature`、`fix`、`docs`、`refactor`、`chore`、`test`;描述用小写英文加短横线,不用中文、下划线或驼峰。详见 [CONTRIBUTING.md#分支命名](CONTRIBUTING.md#分支命名)。

## 改动前必读

- `src/` 是完整 Rust 应用,网页在独立仓库 [disaster-alert-web](https://github.com/luyi2008/disaster-alert-web),本仓不嵌入网页
- 提交前至少跑 `cargo fmt --check`、`cargo check`、`cargo test`;涉及依赖、并发、错误处理、HTTP/WebSocket 或共享模型时再跑 `cargo clippy --all-targets --all-features`
- 新代码不用 `unwrap()`、`expect()`、`dbg!()`、`println!()`、`todo!()`、`unimplemented!()`,不引入 `unsafe`
- 涉及 Bark Key、订阅数据、通知 token 或日志脱敏的改动,先看 CONTRIBUTING.md 的「安全和隐私确认」一节
