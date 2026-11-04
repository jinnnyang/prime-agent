# `PA_TS_REFERENCE`：parity 测试静默跳过，绿灯是假的

## 一句话

`base_url_matches_ts_reference` 这个差分测试在**没有 TS 参考仓库时直接 `return`，然后 Rust 测试框架报告 `test result: ok`**。CI 从不提供该仓库，所以这个断言从未真正执行过，但每次合并门禁都显示通过。

## 机制

`crates/pa-core/src/models/prime_inference.rs:71-98`：

```rust
fn ts_reference_root() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PA_TS_REFERENCE") {
        return path.is_dir().then_some(path);      // 环境变量优先
    }
    let path = home_dir()?.join("prime-agent");    // 兜底 ~/prime-agent
    path.is_dir().then_some(path)
}

#[test]
fn base_url_matches_ts_reference() {
    let Some(root) = ts_reference_root() else {
        eprintln!("SKIPPED: TS reference repo not found (set PA_TS_REFERENCE)");
        return;                                     // ← 静默跳过
    };
    let catalog = root.join("packages/coding-agent/src/core/prime-inference-model-catalog.ts");
    let Ok(source) = std::fs::read_to_string(catalog) else {
        eprintln!("SKIPPED: TS prime-inference-model-catalog.ts unreadable");
        return;                                     // ← 第二道静默跳过
    };
    let expected = /* 从 TS 源码解析 PRIME_INFERENCE_BASE_URL */;
    assert_eq!(PRIME_INFERENCE_BASE_URL, expected);  // ← 只有到这里才算真跑
}
```

`return` 在 Rust 测试里等价于"通过"。`eprintln!` 只在 `--nocapture` 下可见，默认被吞掉。

## 实测证据

本机（无 `~/prime-agent`，`PA_TS_REFERENCE` 未设）：

```
$ cargo test --locked -p pa-core --lib base_url_matches_ts_reference -- --nocapture
running 1 test
SKIPPED: TS reference repo not found (set PA_TS_REFERENCE)
test models::prime_inference::tests::base_url_matches_ts_reference ... ok   ← 注意是 ok
test result: ok. 1 passed; 0 failed
```

CI 侧：全仓库 workflow 搜索 `~/prime-agent` / clone TS 仓库 / `PA_TS_REFERENCE` 设置，**命中数为 0**。`ci.yml` 只设置 `PA_TS_LOCAL_TAG` / `PA_TS_UPSTREAM_*`（那是二进制归档，不是源码 checkout）。所以 CI 上 8 个 shard 里这个测试都没有真跑的理由。

## 为什么这重要

这个测试守的是一条**对外行为契约**：

```
Rust:  PRIME_INFERENCE_BASE_URL = "..."
                    ↕ 必须相等
TS:    export const PRIME_INFERENCE_BASE_URL = "..."
```

AGENTS.md 定义 parity ground truth 就是 TS 实现。如果 Rust 的 base URL 与 TS 不一致，用户从 TS 切到 Rust 会打到不同后端 —— 那不是移植，是分叉。

现在这个守卫是空的：改错了 `PRIME_INFERENCE_BASE_URL`，CI 依然全绿。

## 同类的第二个问题：`ts_reference_root` 的兜底路径

`home_dir()?.join("prime-agent")` 在开发者机器上通常是**另一个 prime-agent 工作目录** —— 也就是 Rust 仓库自己，不是 TS 仓库。这台机器的布局正是如此：

```
C:\Users\jinnn\Documents\prime-agent   ← Rust 仓库（当前 cwd）
C:\Users\jinnn\prime-agent             ← 不存在
```

`~/prime-agent` 不存在 → SKIP。但如果开发者恰好有个名为 `~/prime-agent` 的 Rust checkout（很常见，AGENTS.md 就建议克隆到 `~/prime-agent`），`ts_reference_root()` 会返回它，然后 `read_to_string(packages/coding-agent/...)` 失败 → 走**第二道**静默 skip。两种情况都是 ok。

## 修复方向（未实施）

1. **把 SKIP 变成可见**：`#[ignore]` + 显式 `cargo test -- --ignored`，或引入一个 `PARITY_REQUIRED=1` 开关，CI 里设了就必须真跑，否则 fail。核心是让"没跑"和"跑过"在报告里可区分。
2. **CI 提供源码 checkout**：既然已经有了 `ts-reference-v0.9.8` 二进制归档，同样可以在 CI 里 shallow-clone 上游 `v0.9.8` 的源码到 `$RUNNER_TEMP`，并 `export PA_TS_REFERENCE=...`。成本很小，收益是这条断言真正执行。
3. **兜底路径改掉或加校验**：`~/prime-agent` 这个名字有歧义（Rust 仓库也可能叫这个）。可以改成 `~/prime-agent-ts`，或校验 `packages/coding-agent/` 存在才算命中。

## 相关

- `PA_TS_REFERENCE` 名字上像"参考仓库"，实际上有三种不同用途的路径变量易混：
  - `PA_TS_REFERENCE` — **源码目录**，`pa-core` 单测用，本地开发
  - `PA_TS_ARCHIVE` / `PA_TS_BINARY` — **二进制归档**，`scripts/check_link_frames.py` 用，CI shard 1/8
  - `PA_TS_REFERENCE_TAG` / `PA_TS_UPSTREAM_*` — **下载来源**，`ci.yml` 用
- 二进制那条路已经修好（PR #5，重托管 + 双源回退），它缺归档就**报错**，行为正确
- 这条（源码）缺目录就**静默通过**，行为错误
