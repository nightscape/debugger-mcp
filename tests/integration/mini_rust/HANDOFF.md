# mini-Rust PBT — handoff

This document picks up where the initial scaffolding left off and lists
concrete ways to extend the generation + oracle capability so the PBT
catches more debugger bugs.

## What exists today

- **AST + interpreter + codegen** (`ast.rs`, `interp.rs`, `codegen.rs`):
  small Rust subset (`i64`, `bool`, `String`, `Vec`, `HashMap`, `BTreeMap`),
  observation points with stable IDs.
- **Strategy** (`strategy.rs`): generates well-typed Programs over a fixed
  variable set; round-trip cases pass through interp + codegen. Value pools
  include extended ranges (`±100`, `±1000`) and edge strings (newline,
  embedded quote, ~90-char). `BumpI` uses a separate non-overflow pool.
- **Oracle** (`oracle.rs`): two paths.
  - `via_variables` (DWARF via `debugger_get_variables`) — exercised by the
    PBT today. Drills into `Vec` and `HashMap` via `variablesReference`
    and compares element-by-element. `BTreeMap` is verified by `length`
    only (CodeLLDB exposes the B-tree's internal node graph, not entries).
    String comparison is byte-equal after stripping LLDB's wrapping quotes.
  - `via_eval` — drives a Rust-aware evaluator. Auto-routes through
    `debugger_eval_rust` when registered, falling back to `debugger_evaluate`.
    Not currently called by the PBT harness.
- **End-to-end PBT** (`tests/integration/mini_rust_pbt_test.rs`): drives
  real CodeLLDB; rich failure context (session_state, stack, last 50 output
  lines, snapshot trace through the failing index) on every divergence.
  Stable across 40+ generated programs at the time of writing.
- **Layout probe** (`tests/integration/mini_rust_map_layout_probe.rs`):
  pinned regression test that compiles a fixture, breaks at a known line, and
  **asserts** every layout assumption the oracle makes against
  `debugger_get_variables` output for a populated `HashMap` (incl. edge keys
  with newline + embedded quote), `BTreeMap`, and `Vec`. Runs by default
  (no `#[ignore]`); auto-skips on machines without `codelldb`. If a CodeLLDB
  upgrade changes layout, this fails first with a targeted message pointing
  at exactly which assumption broke. The most load-bearing assertion is
  the "BTreeMap does NOT synthesize `[N]` entries" one — if it ever fires,
  promote the BTreeMap oracle from length-only to entry drill-in.

## Priority extensions

Sorted by **bugs-caught-per-effort**. Each item lists what it tests, where
it goes, and rough size.

### ✅ P0 — Drill into collection contents (DONE for Vec + HashMap)

`Vec` and `HashMap` element-level comparison are landed in
`oracle.rs::{check_vec_children, check_map_children}`. CodeLLDB's actual
layout (captured by `mini_rust_map_layout_probe`):

- **Vec:** children `[0]`..`[n-1]` of element type, plus auxiliary `[raw]`.
  Indexed names parsed via `parse_indexed_name`; aux rows ignored.
- **HashMap:** synthesized pairs `[N]` of type `(K, V)`; drilling into a
  pair gives children `0` (key) and `1` (value). Compared **set-wise**
  because HashMap iteration order is non-deterministic.
- **BTreeMap:** **not** synthesized as indexed entries. CodeLLDB shows
  the B-tree's internal fields (`root`, `length`, `alloc`, `_marker`,
  `[raw]`). Currently we verify `length` only; per-entry comparison
  would require walking the B-tree node graph (still on the wishlist).

If the next person wants order-checked BTreeMap entries, see "P1 —
BTreeMap entry walk" below.

### ✅ P0 — Wire `debugger_eval_rust` into the eval oracle (DONE; tool not yet in main)

`oracle.rs::eval` now picks the first available tool from
`EVAL_TOOL_CANDIDATES` (currently `["debugger_eval_rust", "debugger_evaluate"]`)
based on `ToolsHandler::list_tools()`. As of this writing, only
`debugger_evaluate` is registered in `main` — the Rust-codegen evaluator
lives in `.claude/worktrees/rust-repl-eval`. Once that worktree merges,
the eval oracle automatically picks it up; no further wiring needed.

The eval oracle is *not* currently called by `mini_rust_pbt_test.rs`. To
enable it, swap `check_snapshot_via_variables` for `check_snapshot_via_eval`
(or call both for cross-validation) in `run_one`.

### P1 — Generate control flow

`Stmt::If` and `Stmt::While` are already in the AST but the strategy
doesn't emit them. Adding them surfaces:
- scope-related bugs in conditional branches
- step-into vs step-over semantics on `if` arms
- breakpoint behavior on lines that may or may not execute
- variable lifetime in branches that don't run

**Where:** `strategy.rs::op_strategy` — add an `Op::Block(Vec<Op>)` and
shape it into nested If/While via a recursive strategy. proptest's
`prop_recursive` is built for this.

```rust
fn block_strategy() -> impl Strategy<Value = Vec<Op>> {
    let leaf = op_strategy();
    leaf.prop_recursive(3, 32, 6, |inner| {
        prop_oneof![
            (any_bool_expr(), proptest::collection::vec(inner.clone(), 0..6))
                .prop_map(|(c, body)| Op::If(c, body)),
            (any_bool_expr(), proptest::collection::vec(inner, 0..6))
                .prop_map(|(c, body)| Op::While(c, body)),
        ]
    })
}
```

**Watch out:** `While` needs a guaranteed-terminating condition.
Generate `i < <small const>` and require the body to mutate `i` so the
loop exits. Or cap iterations via a generated counter variable.

**Size:** ~150 lines. ~half a day.

### P1 — Step semantics oracle

Today we only verify breakpoint+continue. A separate test mode would
exercise `debugger_step_over`, `step_into`, `step_out` and verify the
debugger lands on the *next* expected snapshot. This catches stepping
engine bugs that breakpoint-driven tests miss entirely.

**Where:** new `oracle.rs::check_step_sequence` plus a sibling PBT
`mini_rust_step_pbt_test.rs`. Strategy emits programs with simple linear
control flow first (no branches) so step-over has a deterministic next
stop.

**Size:** new file + ~200 lines of harness. ~1 day.

### P1 — Stack trace verification at each snapshot

When generation gains nested functions (P2), each Snapshot can carry the
expected call stack. The oracle calls `debugger_stack_trace` and compares
top-N frames' `function_name` and source line.

**Where:** `interp.rs::Snapshot` add `stack: Vec<FrameRef>`.
`oracle.rs` add `check_stack`.

**Size:** depends on whether nested functions land first.

### P2 — Generate function definitions and calls

Adds:
- `fn helper(args) -> Type { body }` definitions in the AST (a `Program`
  becomes `Vec<FnDef>` plus a `main` body).
- `Expr::Call(name, args)` for invocations.
- Closures aren't necessary at this layer; functions are enough.

This lights up:
- multi-frame stack inspection
- step-into / step-out across frames
- frame-variable scoping (`debugger_get_variables` with `frameId`)
- breakpoints inside helper functions

**Where:** `ast.rs`, `interp.rs` (need a call stack), `codegen.rs`,
`strategy.rs`.

**Size:** ~400 lines across all four files. ~2 days. Big payoff — most
real debugger bugs surface across frames, not within one frame.

### P2 — Enums (`Option`, `Result`, custom)

Production Rust is enum-heavy. The debugger's variant rendering is a
common bug source.

**AST:** `Type::Enum(Vec<Variant>)`, `Expr::EnumCtor(name, variant, args)`,
`Stmt::Match { scrutinee, arms }`.

**Start small:** just `Option<i64>` and `Option<String>`. That alone
already catches `None`/`Some` rendering bugs in the variables view.

**Size:** ~300 lines. ~1.5 days.

### ✅ P2 — Edge values (PARTIALLY DONE; MIN/MAX still pending)

Done: `±1000`, `±100`, `"with\nnewline"`, `"with\"quote"`, ~90-char string.
A separate non-overflowing pool (`SMALL_BUMP_I64S`) feeds `BumpI` so the
extended range doesn't crash interp+codegen.

**Skipped:** `i64::MIN`, `i64::MAX`. Any pairing of `SetI(MAX)` with a
positive `BumpI` overflows raw `i + n` in both the interp and the
generated Rust. Unlocking MIN/MAX requires switching the addition in
`interp::eval_binop` and `codegen::bin_op_str` to `wrapping_add` (and
matching wrapping for sub/mul if/when they are emitted). Estimated
~15 lines once you decide on wrapping vs. saturating semantics — both
sides must agree.

**Skipped (oracle limitation):** an empty-string entry would make the
old substring matcher always match. The new strict matcher
(`prim_value_str_matches` + `strip_lldb_quotes`) handles `""`
correctly, so empty strings are now safe to add when needed.

**Bug found by edge values (and fixed):** the original matcher used
`raw.contains(&format!("{s:?}")) || raw.contains(s)`. The fallback
caused false positives ("beta" inside "alpha-beta-gamma…") and the
debug-format check caused false negatives (CodeLLDB outputs raw newline
bytes; Rust debug-formats them as `\n`). Replaced with strict equality
after stripping LLDB's outer quotes. See unit tests in `oracle.rs`.

### P2 — Nested collections

`Vec<Vec<i64>>`, `HashMap<String, Vec<i64>>`. The AST already permits
this; the interp + codegen partially restrict to `PrimType`. Lift the
restriction.

Catches: recursive pretty-printer bugs, deeply expandable variable trees,
truncation logic in the variables tool.

**Size:** ~100 lines + a bunch of careful matching. ~1 day.

### P3 — Memory pressure

Bias generation toward large containers (1000+ pushes / inserts).
The project already has a dedicated `memory_explosion_test`; PBT can
complement it by varying the *shape* of the large container.

**Where:** `strategy.rs` — add a `large_program_strategy()` variant that
generates 200–2000 push/insert ops in sequence, plus a single observe at
the end. Different test entry point with `PROPTEST_CASES=3` (small, since
each case is heavy).

### P3 — Deliberate panic injection

Generate programs that trigger panics: integer overflow (`i + i64::MAX`
when `i > 0`), `unwrap()` on a `None`. Verify the debugger reports the
panic frame correctly.

Requires: the generated program must be allowed to panic without failing
the test. Wrap the SUT execution in a "panic is expected" mode.

### P3 — Conditional breakpoints

The project supports breakpoint conditions (per `bb3ccc6 feat: Add
breakpoint conditions`). Generation already produces loops once P1 lands
— add a test mode that sets a conditional breakpoint (e.g., `i == 3`)
and verifies the SUT only stops on the matching iteration.

### P3 — Differential testing across adapters

Run the same generated program through CodeLLDB and (when configured)
plain `lldb-vscode` / `lldb-dap`. Diff the snapshot streams. Adapter
divergence is a real source of "works on Linux, breaks on macOS" bugs.

**Where:** parameterize `run_one` over an adapter spec; loop the test
over both.

## Harness improvements

### ✅ Capture richer failure context (DONE)

`mini_rust_pbt_test.rs::gather_failure_context` dumps `debugger_session_state`,
`debugger_stack_trace`, recent `debugger_get_output` entries, and the full
snapshot trace through the failing index. Wired into both the
"unexpected termination" and "snapshot mismatch" failure paths in `run_one`.

### P1 — BTreeMap entry walk

Drill into `t.root` recursively and walk the B-tree to enumerate entries.
The probe shows `root` is `Option<NodeRef<...>>`; CodeLLDB exposes it as
a struct with `height` and a `node` pointer. Walking that requires
following the `node` pointer's `keys`/`vals`/`edges` arrays per height.

**Why bother:** would catch ordering bugs and per-entry corruption, neither
of which the `length`-only check sees. **Why hard:** B-tree internals are
private and change between Rust toolchains; the synthesizer in CodeLLDB
intentionally exposes only what's stable. A robust walk would need to be
gated on toolchain version.

**Cheaper alternative:** convert observation snapshots to also store an
expected `Vec` view of the BTreeMap entries (sorted by key), then add a
secondary observe that does `let view: Vec<_> = t.iter().collect();` so the
SUT-side data structure becomes a Vec we already drill into. ~50 lines
in `codegen.rs` + the strategy's keepalive section.

### Persist failures as a regression corpus

proptest already supports failure persistence via `PROPTEST_REGRESSION_FILE`.
Wire it: when a case fails, save the *generated source* (not just the
proptest seed) under `tests/regressions/mini_rust/<hash>.rs` and a
companion `<hash>.json` with the expected snapshots. Then add a separate
non-PBT test that loads each corpus file and runs it like a static
fixture. Re-running the corpus protects against regressions even when
proptest seeds become stale across versions.

### Reuse sessions across iterations

Spawning a fresh CodeLLDB per iteration is the main runtime cost. With
careful state cleanup (disconnect + clear breakpoints) sessions can be
reused. Risk: state leaks across cases, masking bugs. **Don't do this
until the rest of the harness is solid.**

### Per-iteration time budget

Each iteration currently runs unbounded. Add a `tokio::time::timeout`
around the whole `run_one` body (e.g., 30s). Surface latency regressions
and prevent CI hangs.

## Known traps for the next person

1. **`gen` is reserved in Rust 2024** — even on edition 2021, rust-analyzer
   complains. Don't name a binding `gen`.

2. **Consecutive `let _: () = ();` collapse to one breakpoint** — that's
   why we use `std::hint::black_box(())` as the observation sentinel. If
   you change the sentinel, verify two adjacent observes are still
   distinct stops.

3. **LLDB drops locals at the line just before `}`** — the strategy adds
   keepalive references *after* the final observe. If you remove or
   reorder them, the final snapshot will report all variables as missing.

4. **`debugger_evaluate` is C++/Swift expression syntax**, not Rust. The
   `via_eval` oracle is intentionally pre-built for the future
   `debugger_eval_rust` tool. Don't try to reach into it from C++ syntax —
   you'll fight LLDB's parser.

5. **Strategy doesn't enforce termination of `while`** yet — once you add
   loop generation, decrement-and-bound the condition or you'll get
   non-terminating PBT cases.

6. **Map child layout in CodeLLDB varies by version.** The current Hash/BTree
   parsers were written against the layout captured by
   `mini_rust_map_layout_probe` (run it on upgrade). HashMap pairs use names
   `[0]`..`[n-1]` plus an auxiliary `[raw]` row; the parser filters by
   `[<digits>]` and ignores everything else (so adding new aux rows doesn't
   break us). BTreeMap exposes B-tree internals (`root`, `length`, …) — we
   only read `length`.

7. **CodeLLDB does not escape strings; `format!("{s:?}")` does.** A real
   newline byte in a Rust `String` shows up as a real newline in LLDB's
   `value` field; Rust debug-format would render it `\n`. The oracle's
   `prim_value_str_matches` strips LLDB's outer quotes and compares bytes
   exactly. If you ever need substring/regex matching against LLDB output,
   *don't* go back to `raw.contains(s)` — that's the false-positive trap
   that masked Map mismatches before the strict matcher landed.

8. **`debugger_evaluate` uses C++/Swift expression syntax**, not Rust.
   `oracle.rs::eval` auto-routes through `debugger_eval_rust` when present;
   if you call `debugger_evaluate` directly with Rust syntax (`v.len() as i64`,
   `name.as_str()`) it will fail.

## Quick reference

- Run smoke tests (incl. oracle unit tests): `cargo test --test mini_rust_smoke_test`
- Run PBT (real CodeLLDB): `PROPTEST_CASES=12 cargo test --test mini_rust_pbt_test -- --ignored --nocapture`
- Layout regression test (auto-skips without CodeLLDB): `cargo test --test mini_rust_map_layout_probe`
- Generated sources live at: `tests/fixtures/target/mini_rust_pbt/<iter>/prog.rs`
- Reference state structure: `Snapshot { line, vars: BTreeMap<String, Value> }`
- Mismatch types: see `oracle.rs::Mismatch` enum.
- 4 PBT iterations ≈ 7s wall; 40 iterations ≈ 65s wall on M-series Mac.
