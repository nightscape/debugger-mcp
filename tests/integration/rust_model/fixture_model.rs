use super::reference_state::ExpectedStop;

/// Lines in debug_exercise.rs where breakpoints are valid (executable lines).
pub const BREAKABLE_LINES: &[i32] = &[
    8, 9,           // add()
    13, 14, 16, 18, 20, // classify()
    25,             // process_point()
    29, 30, 31, 33, // iterate()
    36, 37, 38, 39, 40, 41, 42, 43, 44, // main()
];

/// Lines inside the iterate() loop body, useful for conditional breakpoint tests.
pub const LOOP_BODY_LINE: i32 = 31;

/// Entry points of each function.
pub const ADD_ENTRY: i32 = 8;
pub const CLASSIFY_ENTRY: i32 = 13;
pub const PROCESS_POINT_ENTRY: i32 = 25;
pub const ITERATE_ENTRY: i32 = 29;
pub const MAIN_ENTRY: i32 = 36;

/// Expected trace when stepping through main() with step-over.
/// These are the lines main() touches (calls are opaque with step-over).
/// NOTE: Actual line stops depend on CodeLLDB behavior. This will be
/// calibrated by the calibration test and updated accordingly.
pub fn main_step_over_trace() -> Vec<ExpectedStop> {
    vec![
        ExpectedStop {
            line: 36,
            function_name: "main",
            variables: vec![],
            min_stack_depth: 1,
        },
        ExpectedStop {
            line: 37,
            function_name: "main",
            variables: vec![("a", "3")],
            min_stack_depth: 1,
        },
        ExpectedStop {
            line: 38,
            function_name: "main",
            variables: vec![("a", "3"), ("b", "5")],
            min_stack_depth: 1,
        },
        ExpectedStop {
            line: 39,
            function_name: "main",
            variables: vec![("a", "3"), ("b", "5"), ("c", "8")],
            min_stack_depth: 1,
        },
        ExpectedStop {
            line: 40,
            function_name: "main",
            variables: vec![],
            min_stack_depth: 1,
        },
        ExpectedStop {
            line: 41,
            function_name: "main",
            variables: vec![],
            min_stack_depth: 1,
        },
        ExpectedStop {
            line: 42,
            function_name: "main",
            variables: vec![],
            min_stack_depth: 1,
        },
        ExpectedStop {
            line: 43,
            function_name: "main",
            variables: vec![],
            min_stack_depth: 1,
        },
        ExpectedStop {
            line: 44,
            function_name: "main",
            variables: vec![("result", "52")],
            min_stack_depth: 1,
        },
    ]
}

/// Expected state when stopped inside add() after step-into from main line 38.
pub fn add_entry_stop() -> ExpectedStop {
    ExpectedStop {
        line: ADD_ENTRY,
        function_name: "add",
        variables: vec![("a", "3"), ("b", "5")],
        min_stack_depth: 2,
    }
}

/// Expected state after stepping out of add() back to main.
pub fn add_return_stop() -> ExpectedStop {
    ExpectedStop {
        line: 38,
        function_name: "main",
        variables: vec![],
        min_stack_depth: 1,
    }
}

/// Expected state when stopped at iterate loop body with i == 3.
pub fn iterate_conditional_stop() -> ExpectedStop {
    ExpectedStop {
        line: LOOP_BODY_LINE,
        function_name: "iterate",
        // When i == 3: total = 0 + 1 + 2 = 3
        variables: vec![("i", "3"), ("total", "3")],
        min_stack_depth: 2,
    }
}
