# 工具链未锁定导致 clippy lint 名跨版本漂移

## 背景

`ci.yml` 的合并门禁用 `dtolnay/rust-toolchain@ce67845...`（SHA 固定为 **1.98.1**），但仓库**没有** `rust-toolchain.toml`。结果是：

- CI 跑 1.98.1
- 本地开发者跑自己的 stable（本机是 1.96）

两个版本的 clippy 对 `unused_async_trait_impl` 这条 lint 的存在性判断相反。

## 具体表现

`crates/pa-telemetry/src/sinks/file.rs` 的 `set_private`：

```rust
#[cfg_attr(not(unix), allow(clippy::unused_async))]
async fn set_private(&self) -> Result<()> { ... }
```

| 工具链 | 行为 |
|---|---|
| clippy 1.96 | `unused_async_trait_impl` 已并入 `unused_async`；此写法通过 |
| clippy 1.98.1 | `unused_async_trait_impl` 仍是独立 lint；报 `error: unused async for async trait impl function with no .await statements` → `-D warnings` 下构建失败 |

`4e536df25`（2026-10-26）基于本地 1.96 的判断删掉了旧名，正因此把 CI 门禁弄红。该提交的 message 里写着 "The local clippy (1.96) no longer knows ... unused_async_trait_impl" —— 这个前提对 CI 不成立。

## 实测三个组合

用 `rustup toolchain install 1.98.1` + `rustup component add clippy rustfmt` 后真机验证：

| 写法 | clippy 1.96 | clippy 1.98.1 |
|---|---|---|
| 只 allow `unused_async` | ✅ | ❌ 门禁红 |
| 只 allow `unused_async_trait_impl` | ❌ `unknown lint` | ✅ |
| **两个都 allow** | ❌ `unknown lint` | ✅ |
| 两个都 allow + `#[allow(unknown_lints)]` | ✅ | ✅ |

结论：**没有一种 allow 写法能同时通过两个版本**，除非加上 `unknown_lints` 掩盖。

## 曾被采用的掩盖式修复（已撤销）

先试过：

```rust
#[allow(unknown_lints)]                    // ← 掩耳盗铃
#[cfg_attr(not(unix), allow(clippy::unused_async, clippy::unused_async_trait_impl))]
```

这在两个版本上都"通过"，但代价是：**`unknown_lints` 会静音整个表达式范围内所有拼错的 lint 名**。也就是说以后任何人把 `clippy::unused_async` 打成 `clippy::unused_asnyc`，这里都不会报错，lint 会静默失效。

这个方案被否决，因为它用"看不见的失效"换"看得见的绿"。

## 采用的根因修复

新增 `rust-toolchain.toml`：

```toml
[toolchain]
channel = "1.98.1"
```

效果：无论开发者本机装了什么 stable，在仓库目录里 `cargo` 都会用 1.98.1 —— 与 CI 门禁完全一致。

同时把 allow 写成两个名字：

```rust
#[cfg_attr(not(unix), allow(clippy::unused_async, clippy::unused_async_trait_impl))]
```

在 1.98.1 上 `unused_async_trait_impl` 是真实 lint，两个名字都在用，不需要 `unknown_lints`。旧工具链若绕过 pin 直接调用，才会撞上 `unknown lint` —— 这是**显式失败**，比静默失效正确。

## 仍需解决的遗留

1. **`unused_async_trait_impl` 未来可能再变**。上游已把它标记为合并目标，1.99+ 可能真的移除。届时 `rust-toolchain.toml` 升版时这条 allow 会变成 `unknown lint` 而**显式报错** —— 这是预期的安全行为，修法是删掉旧名。
2. **`rust-toolchain.toml` 与 CI 的 SHA pin 现在是两处真相**。`ci.yml` 里 `dtolnay/rust-toolchain@ce678459...` 的 SHA 与 `rust-toolchain.toml` 的 `"1.98.1"` 必须同步升级，否则本地和 CI 又会分叉。后续可以改成 workflow 读 `rust-toolchain.toml`，让真相只有一处。
3. **fork 脱离上游后**，`ci.yml` 里 `PrimeIntellect-ai/prime-agent` 的 TS 参考下载（v0.9.8）会失效。届时需要把 v0.9.8 的 `prime-agent-0.9.8-linux-x64.tar.gz` 复制到本 fork 的 release，或改用 `PA_TS_REFERENCE` 指向本地 checkout。

## 相关

- 修复 PR：#3
- 引入问题的提交：`4e536df25`（删 lint 名）、`2f4b8fc8d`（e2e 断言测空替身）
- 同类问题：`crates/pa-tui/src/input.rs` 的 `from_secs(60)` 在 1.96 报 `duration_suboptimal_units`、1.98.1 不报 —— 同样是版本漂移，已顺手统一为 `from_mins(1)`（值等价）
