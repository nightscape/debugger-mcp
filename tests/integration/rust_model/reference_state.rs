#[derive(Debug, Clone)]
pub struct ExpectedStop {
    pub line: i32,
    pub function_name: &'static str,
    pub variables: Vec<(&'static str, &'static str)>,
    pub min_stack_depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelState {
    Stopped,
    Running,
    Terminated,
}
