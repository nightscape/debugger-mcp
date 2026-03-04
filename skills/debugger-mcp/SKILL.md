---
name: debugger-mcp
description: >
  Debug Rust programs using the debugger_mcp MCP server (DAP over LLDB).
  Use when the user asks to debug a Rust test, binary, or program interactively —
  setting breakpoints, stepping through code, inspecting variables.
  Triggers on "debug", "debugger", "breakpoint", "step through", "inspect variable",
  or "/debugger-mcp".
---

# Rust Debugging via debugger_mcp

Debug Rust binaries interactively using the `debugger_mcp` MCP server, which wraps
LLDB via the Debug Adapter Protocol (DAP).

## CRITICAL RULES
- **NEVER** call `debugger_disconnect` without asking the user first.
  Once disconnected, all stdout/stderr output is permanently lost.

## Prerequisites

- The `debugger_mcp` MCP server must be configured and running
- The Rust binary must be compiled with debug info (`cargo build` or `cargo test --no-run`)

## Workflow

### 1. Build the test binary (no run)

```bash
cargo test --package <pkg> --test <test_name> --no-run 2>&1 | tail -5
```
Output shows: `Executable tests/foo.rs (target/debug/deps/foo-abc123def)`

### 2. Launch the debug session

```
mcp__debugger_mcp__debugger_start(
  language: "rust",
  program: "/absolute/path/to/binary",
  args: ["test_name_filter", "--test-threads=1", "--nocapture"],
  cwd: "/workspace/root",
  stopOnEntry: true
)
```

Returns `sessionId`. Save it for all subsequent calls.

**Note:** `stopOnEntry: true` stops at `_dyld_start` (the dynamic linker), NOT
at `main`. You must set a real breakpoint and `continue` to reach your code.

### 3. Set breakpoints and continue to them

```
mcp__debugger_mcp__debugger_set_breakpoint(
  sessionId: "...",
  sourcePath: "/absolute/path/to/source.rs",
  line: 42
)
mcp__debugger_mcp__debugger_continue(sessionId: "...")
mcp__debugger_mcp__debugger_wait_for_stop(sessionId: "...", timeoutMs: 30000)
```

`wait_for_stop` blocks efficiently until the breakpoint hits — no polling needed.

### 4. Inspect state

Get the stack trace first (needed for frameId):
```
mcp__debugger_mcp__debugger_stack_trace(sessionId: "...")
```
Use `stackFrames[0].id` as the `frameId` for evaluate calls.

### 5. Evaluate expressions

```
mcp__debugger_mcp__debugger_evaluate(
  sessionId: "...",
  expression: "my_variable.field",
  frameId: <from stack trace>
)
```

### 6. Step through code

```
mcp__debugger_mcp__debugger_step_over(sessionId: "...")
mcp__debugger_mcp__debugger_wait_for_stop(sessionId: "...")
# Then get fresh stack trace + frame IDs before evaluating
```

### 7. Clean up

**IMPORTANT:** Always ask the user before disconnecting. Once disconnected, stdout/stderr
output from the debugged program is lost and cannot be retrieved.

```
mcp__debugger_mcp__debugger_disconnect(sessionId: "...")
```

## Auto-Expansion of Collections

The patched debugger_mcp (nightscape/debugger-mcp)
auto-expands collections via LLDB's synthetic providers. Just evaluate the variable
directly — no manual pointer arithmetic needed:

| Type | Expression | Output |
|---|---|---|
| `HashMap<K,V>` | `my_map` | `size=N` + all key-value pairs |
| `Vec<T>` | `my_vec` | `size=N` + all elements with fields |
| `BTreeMap<K,V>` | `my_btree` | All key-value pairs |
| `BTreeSet<T>` | `my_set` | All elements |
| `String` | `my_string` | `"actual content"` (no char expansion) |
| Enum | `my_enum` | Variant name: `Text`, `Source` |
| Newtype | `my_newtype` | Inner fields expanded |
| `Option<T>` | `my_option` | `None` or inner value |

Expansion depth is 2 levels. `[raw]` LLDB internals and char-by-char string
noise are filtered out automatically.

## LLDB Expression Syntax for Rust

LLDB uses a C++ expression evaluator. Rust syntax does NOT work directly.

### Scalars and simple fields

| What | Expression | Notes |
|---|---|---|
| Struct field (value) | `my_struct.field` | Works directly |
| Struct field (pointer) | `my_ptr->field` | Use `->` for pointers |
| Vec length | `my_vec.len` | Field access, NOT `.len()` |
| Enum variant | `my_block.content_type` | Shows variant name: `Text`, `Source` |
| bool/int/float | `my_var` | Returns value directly |

### Tuple struct / newtype fields

Use `__0`, `__1` etc. instead of `.0`, `.1` (LLDB interprets `.0` as float):

```
my_entity_uri.__0.val    // Access inner field of EntityUri(Uri<String>)
```

### Manual Vec<T> element access (fallback)

If auto-expansion doesn't show enough detail, you can manually index:

**Step 1 — Get the raw data pointer:**
```
(void*)my_vec.buf.inner.ptr.pointer.pointer
```
Returns an address like `0x0000000a85094000`.

**Step 2 — Cast to element type and index:**
```
((my::module::MyType*)0xADDRESS)[index].field
```

### Manual String access (fallback)

If you need the raw char pointer:
```
(char*)my_string.vec.buf.inner.ptr.pointer.pointer
```

### What does NOT work

- **Method calls**: `.len()`, `.is_empty()`, `.get(0)` — LLDB can't call Rust methods
- **Rust indexing**: `vec[0]` — LLDB doesn't have a subscript operator for Vec
- **`.0` tuple access**: interpreted as float `0.0` — use `.__0` instead
- **Complex format strings**: no `format!`, `dbg!`, etc.

## Gotchas

1. **Frame IDs change between stops** — after continue/step, ALWAYS get a fresh
   `stack_trace` before calling `evaluate`. Old frame IDs are invalid.

2. **stopOnEntry stops at dyld** — not at your code. Set a breakpoint and continue.

3. **Breakpoints can be added anytime** — no need to disconnect and restart.
   Call `set_breakpoint` while stopped (or even running) and it takes effect
   immediately. Use `list_breakpoints` to see all active breakpoints.

4. **Proptest/PBT loops** — breakpoints hit on every proptest case. Use early
   cases to orient, then refine. The breakpoint stays set across iterations.

5. **wait_for_stop timeout** — for long operations (parsing, compilation), use
   a longer `timeoutMs` (e.g., 30000).

## Example: Debug a PBT Test

```
# 1. Build
cargo test -p holon-orgmode --test round_trip_pbt --no-run 2>&1 | tail -5

# 2. Launch
debugger_start(language: "rust",
  program: "/path/to/target/debug/deps/round_trip_pbt-abc123",
  args: ["test_round_trip", "--test-threads=1", "--nocapture"],
  stopOnEntry: true)

# 3. Wait for dyld entry, set breakpoint, continue
debugger_wait_for_stop()
debugger_set_breakpoint(sourcePath: "/.../round_trip_pbt.rs", line: 720)
debugger_continue()
debugger_wait_for_stop()  # Now at your breakpoint

# 4. Inspect
debugger_stack_trace()  # Get frameId
debugger_evaluate(expression: "blocks.len", frameId: 1001)
# Get Vec pointer
debugger_evaluate(expression: "(void*)blocks.buf.inner.ptr.pointer.pointer", frameId: 1001)
# Index into Vec
debugger_evaluate(expression: "((holon_api::block::Block*)0xADDR)[0].content_type", frameId: 1001)

# 5. Step and inspect
debugger_step_over()
debugger_wait_for_stop()
debugger_stack_trace()  # Fresh frame IDs!
debugger_evaluate(expression: "parse_result.blocks.len", frameId: <new_id>)

# 6. Clean up
debugger_disconnect()
```
