// Fixture designed to trigger memory explosion in debug adapters.
//
// Contains data structures that, when expanded by LLDB's synthetic
// providers without a `count` limit, cause the adapter to materialise
// thousands of children per variable — leading to multi-GB RSS.
//
// The program itself uses only ~10 MB at runtime.  The explosion
// happens inside the debugger when it tries to inspect the variables.

use std::collections::HashMap;

struct Node {
    value: i32,
    next: Option<Box<Node>>,
}

fn build_linked_list(n: i32) -> Box<Node> {
    let mut head = Box::new(Node { value: 0, next: None });
    for i in 1..n {
        head = Box::new(Node { value: i, next: Some(head) });
    }
    head
}

fn main() {
    // Large Vec — 10k elements.  Expanding all children = 10k variables requests.
    let big_vec: Vec<i32> = (0..10_000).collect();

    // HashMap with 5k entries — each entry has key + value children.
    let big_map: HashMap<String, Vec<u8>> = (0..5_000)
        .map(|i| (format!("key_{}", i), vec![i as u8; 64]))
        .collect();

    // Linked list with 1k nodes — recursive expansion is O(depth).
    let list = build_linked_list(1_000);

    // Nested: Vec<HashMap<String, Vec<u8>>> — depth-2 expansion is N×M.
    let nested: Vec<HashMap<String, Vec<u8>>> = (0..100)
        .map(|i| {
            (0..100)
                .map(|j| (format!("{}_{}", i, j), vec![j as u8; 32]))
                .collect()
        })
        .collect();

    // Breakpoint target — all variables are in scope here.
    let total = big_vec.len() + big_map.len() + nested.len();
    println!("total containers: {}", total); // line 47
    println!("list head: {}", list.value);
    println!("done");
}
