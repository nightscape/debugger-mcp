//! Detection of compiler-generated async/coroutine state-machine types.
//!
//! Background: when stopped inside an `async fn` past one or more `.await`
//! suspension points, the locals view exposes the compiler-generated
//! `{async_fn_env#N}` (or `{coroutine_env#N}`, or legacy `{generator#N}`)
//! type. That type is a *tagged union* of every suspension state. CodeLLDB's
//! synthetic formatter materialises children for every variant, which under
//! nested async fns × many await points can produce hundreds of captured
//! locals and OOM the adapter (see
//! `tests/integration/async_state_machine_pbt_test.rs`).
//!
//! When we detect a drill into one of these types, we route through
//! `evaluate("?path", context: "watch")` instead of the normal `variables`
//! request — CodeLLDB honors `?` (the showRaw prefix) on evaluate but not
//! on the variables request as of 1.11.

/// Returns true if `type_name` is a compiler-generated async/coroutine
/// state-machine wrapper that needs the `no_synthetic` escape hatch when
/// expanded. Matches all rustc naming variants we know about; the patterns
/// are conservative substrings rather than anchored regexes so future
/// rustc renames are likely to keep matching at least one form.
pub fn is_state_machine_type(type_name: &str) -> bool {
    // rustc state-machine type-name forms across versions.
    // - `{async_fn_env#0}`: current `async fn` body.
    // - `{async_block_env#0}`: `async { ... }` block.
    // - `{coroutine_env#0}` / `{coroutine#0}`: generic coroutine layer.
    // - `{generator#0}`: pre-coroutine legacy name.
    // Both bare and prefixed forms are matched — type-name strings may
    // come back as `core::future::...::{async_fn_env#0}` (path-prefixed)
    // or with extra angle-bracket parameters.
    const NEEDLES: &[&str] = &[
        "{async_fn_env#",
        "{async_block_env#",
        "{coroutine_env#",
        "{coroutine#",
        "{generator#",
    ];
    NEEDLES.iter().any(|n| type_name.contains(n))
}

/// Returns true if `frame_name` is a Rust closure frame produced by an
/// `async fn` / `async {}` block. The frame names rustc emits look like:
///   - `crate::path::fn::{closure#0}`         (current rustc, post-2022)
///   - `crate::path::fn::{{closure}}`         (legacy rendering)
///   - `<crate::path::fn::{closure#0} as core::future::Future>::poll`
///     (when LLDB displays the trait-impl path)
///
/// We match on the substring `{closure#` or `{{closure}}` — both are emitted
/// only by the compiler and never appear in user-named items.
///
/// **Why this matters separately from `is_state_machine_type`:**
/// `is_state_machine_type` looks at a *variable's type name*. This function
/// looks at the *frame name* — i.e., we're standing inside a state-machine
/// closure right now. Auto-enrichment fetches the locals scope of that frame
/// before any user tool call, and CodeLLDB computes per-Variable summary
/// strings during the response (walking each captured container's synthetic
/// provider) regardless of `format.showRaw=true`. The result: the initial
/// stop can hang or OOM before the agent gets a chance to drill anything.
/// Skipping locals enrichment when we're in such a frame keeps the session
/// responsive; the agent can still call `debugger_get_variables` explicitly
/// with `noSynthetic: true` when it actually needs them.
pub fn is_async_closure_frame(frame_name: &str) -> bool {
    frame_name.contains("{closure#") || frame_name.contains("{{closure}}")
}

/// Extend a parent expression path with a child's name, choosing the right
/// syntax for indexed (`[0]`) vs named (`.field`) access. Used to construct
/// expressions that `evaluate` can resolve when we route around the variables
/// request.
pub fn extend_path(parent: &str, child_name: &str) -> String {
    if child_name.starts_with('[') {
        format!("{parent}{child_name}")
    } else if parent.is_empty() {
        child_name.to_string()
    } else {
        format!("{parent}.{child_name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_current_async_fn_env() {
        assert!(is_state_machine_type("{async_fn_env#0}"));
        assert!(is_state_machine_type("{async_fn_env#42}"));
        assert!(is_state_machine_type(
            "core::future::from_generator::GenFuture<myapp::foo::{async_fn_env#0}>"
        ));
    }

    #[test]
    fn detects_async_block_env() {
        assert!(is_state_machine_type("{async_block_env#0}"));
    }

    #[test]
    fn detects_coroutine_variants() {
        assert!(is_state_machine_type("{coroutine_env#0}"));
        assert!(is_state_machine_type("{coroutine#3}"));
    }

    #[test]
    fn detects_legacy_generator() {
        assert!(is_state_machine_type("{generator#0}"));
    }

    #[test]
    fn rejects_user_types() {
        assert!(!is_state_machine_type("Vec<i64>"));
        assert!(!is_state_machine_type("alloc::collections::BTreeMap<i64, i64>"));
        assert!(!is_state_machine_type("std::collections::HashMap<String, i64>"));
        assert!(!is_state_machine_type("&str"));
        assert!(!is_state_machine_type(""));
    }

    #[test]
    fn rejects_user_types_that_mention_async_word() {
        // Defensive: a type *named* "MyAsyncThing" should not be flagged.
        // The needle requires the `{...#` punctuation, which only rustc emits.
        assert!(!is_state_machine_type("MyAsyncThing"));
        assert!(!is_state_machine_type("async_helper::Future"));
    }

    #[test]
    fn detects_async_closure_frame_names() {
        assert!(is_async_closure_frame("crate::apply_split_block::{closure#0}"));
        assert!(is_async_closure_frame("apply_split_block::{{closure}}"));
        assert!(is_async_closure_frame(
            "<crate::path::fn::{closure#0} as core::future::Future>::poll"
        ));
    }

    #[test]
    fn rejects_non_closure_frames() {
        assert!(!is_async_closure_frame("main"));
        assert!(!is_async_closure_frame("std::sys::unix::thread::Thread::new"));
        assert!(!is_async_closure_frame("E2ESut::apply_split_block"));
        assert!(!is_async_closure_frame(""));
    }

    #[test]
    fn extends_named_path() {
        assert_eq!(extend_path("state", "c"), "state.c");
        assert_eq!(extend_path("state.c", "len"), "state.c.len");
    }

    #[test]
    fn extends_indexed_path() {
        assert_eq!(extend_path("xs", "[0]"), "xs[0]");
        assert_eq!(extend_path("state.c", "[42]"), "state.c[42]");
    }

    #[test]
    fn extends_from_empty_root() {
        // First-level expansion: parent path is empty (we're under a scope).
        assert_eq!(extend_path("", "buf"), "buf");
    }
}
