use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DebugState {
    NotStarted,
    Initializing,
    Initialized,
    Launching,
    Running,
    Stopped { thread_id: i32, reason: String },
    Terminated,
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Breakpoint {
    pub source_path: String,
    pub line: i32,
    pub id: Option<i32>,
    pub verified: bool,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

/// A source file + line location, used to identify breakpoints by position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BreakpointLocation {
    pub source_path: String,
    pub line: i32,
}

/// A breakpoint that remains dormant until a specified dependency breakpoint is hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependentBreakpoint {
    pub source_path: String,
    pub line: i32,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub activate_after: BreakpointLocation,
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub state: DebugState,
    pub breakpoints: HashMap<String, Vec<Breakpoint>>,
    pub threads: Vec<i32>,
    /// Breakpoints waiting for a dependency to be hit before activation.
    pub dependent_breakpoints: Vec<DependentBreakpoint>,
    /// Breakpoint locations that have been hit at least once during this session.
    pub hit_locations: HashSet<BreakpointLocation>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            state: DebugState::NotStarted,
            breakpoints: HashMap::new(),
            threads: Vec::new(),
            dependent_breakpoints: Vec::new(),
            hit_locations: HashSet::new(),
        }
    }

    pub fn set_state(&mut self, state: DebugState) {
        self.state = state;
    }

    pub fn add_breakpoint(
        &mut self,
        source: String,
        line: i32,
        condition: Option<String>,
        hit_condition: Option<String>,
    ) {
        let bp = Breakpoint {
            source_path: source.clone(),
            line,
            id: None,
            verified: false,
            condition,
            hit_condition,
        };

        self.breakpoints.entry(source).or_default().push(bp);
    }

    pub fn update_breakpoint(&mut self, source: &str, line: i32, id: i32, verified: bool) {
        if let Some(bps) = self.breakpoints.get_mut(source) {
            if let Some(bp) = bps.iter_mut().find(|b| b.line == line) {
                bp.id = Some(id);
                bp.verified = verified;
            }
        }
    }

    pub fn remove_breakpoint(&mut self, source: &str, line: i32) -> bool {
        let removed = if let Some(bps) = self.breakpoints.get_mut(source) {
            let len_before = bps.len();
            bps.retain(|bp| bp.line != line);
            bps.len() < len_before
        } else {
            false
        };
        if removed {
            if let Some(bps) = self.breakpoints.get(source) {
                if bps.is_empty() {
                    self.breakpoints.remove(source);
                }
            }
        }
        removed
    }

    pub fn get_breakpoints(&self, source: &str) -> Vec<Breakpoint> {
        self.breakpoints.get(source).cloned().unwrap_or_default()
    }

    pub fn add_thread(&mut self, thread_id: i32) {
        if !self.threads.contains(&thread_id) {
            self.threads.push(thread_id);
        }
    }

    pub fn add_dependent_breakpoint(
        &mut self,
        source_path: String,
        line: i32,
        condition: Option<String>,
        hit_condition: Option<String>,
        activate_after: BreakpointLocation,
    ) {
        self.dependent_breakpoints.push(DependentBreakpoint {
            source_path,
            line,
            condition,
            hit_condition,
            activate_after,
        });
    }

    pub fn remove_dependent_breakpoint(&mut self, source_path: &str, line: i32) -> bool {
        let len_before = self.dependent_breakpoints.len();
        self.dependent_breakpoints
            .retain(|bp| !(bp.source_path == source_path && bp.line == line));
        self.dependent_breakpoints.len() < len_before
    }

    /// Record that a breakpoint at this location was hit.
    pub fn record_breakpoint_hit(&mut self, source_path: &str, line: i32) {
        self.hit_locations.insert(BreakpointLocation {
            source_path: source_path.to_string(),
            line,
        });
    }

    /// Returns dependent breakpoints whose dependency has now been satisfied,
    /// removing them from the dependent list.
    pub fn take_activated_dependents(&mut self) -> Vec<DependentBreakpoint> {
        let hit = &self.hit_locations;
        let mut activated = Vec::new();
        let mut remaining = Vec::new();
        for bp in self.dependent_breakpoints.drain(..) {
            if hit.contains(&bp.activate_after) {
                activated.push(bp);
            } else {
                remaining.push(bp);
            }
        }
        self.dependent_breakpoints = remaining;
        activated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_state_new() {
        let state = SessionState::new();
        assert!(matches!(state.state, DebugState::NotStarted));
        assert!(state.breakpoints.is_empty());
        assert!(state.threads.is_empty());
    }

    #[test]
    fn test_set_state() {
        let mut state = SessionState::new();
        state.set_state(DebugState::Running);
        assert!(matches!(state.state, DebugState::Running));
    }

    #[test]
    fn test_add_breakpoint() {
        let mut state = SessionState::new();
        state.add_breakpoint("test.py".to_string(), 10, None, None);

        let bps = state.get_breakpoints("test.py");
        assert_eq!(bps.len(), 1);
        assert_eq!(bps[0].line, 10);
        assert!(!bps[0].verified);
    }

    #[test]
    fn test_update_breakpoint() {
        let mut state = SessionState::new();
        state.add_breakpoint("test.py".to_string(), 10, None, None);
        state.update_breakpoint("test.py", 10, 1, true);

        let bps = state.get_breakpoints("test.py");
        assert_eq!(bps[0].id, Some(1));
        assert!(bps[0].verified);
    }

    #[test]
    fn test_add_thread() {
        let mut state = SessionState::new();
        state.add_thread(1);
        state.add_thread(2);
        state.add_thread(1); // Duplicate should not be added

        assert_eq!(state.threads.len(), 2);
        assert!(state.threads.contains(&1));
        assert!(state.threads.contains(&2));
    }

    #[test]
    fn test_remove_breakpoint() {
        let mut state = SessionState::new();
        state.add_breakpoint("test.py".to_string(), 10, None, None);
        state.add_breakpoint("test.py".to_string(), 20, None, None);

        assert!(state.remove_breakpoint("test.py", 10));
        let bps = state.get_breakpoints("test.py");
        assert_eq!(bps.len(), 1);
        assert_eq!(bps[0].line, 20);
    }

    #[test]
    fn test_remove_breakpoint_last_cleans_up_entry() {
        let mut state = SessionState::new();
        state.add_breakpoint("test.py".to_string(), 10, None, None);

        assert!(state.remove_breakpoint("test.py", 10));
        assert!(state.breakpoints.is_empty());
    }

    #[test]
    fn test_remove_breakpoint_nonexistent() {
        let mut state = SessionState::new();
        assert!(!state.remove_breakpoint("test.py", 10));
    }

    #[test]
    fn test_get_breakpoints_empty() {
        let state = SessionState::new();
        let bps = state.get_breakpoints("nonexistent.py");
        assert!(bps.is_empty());
    }

    #[test]
    fn test_debug_state_stopped() {
        let state = DebugState::Stopped {
            thread_id: 1,
            reason: "breakpoint".to_string(),
        };

        if let DebugState::Stopped { thread_id, reason } = state {
            assert_eq!(thread_id, 1);
            assert_eq!(reason, "breakpoint");
        } else {
            panic!("Expected Stopped state");
        }
    }

    #[test]
    fn test_add_dependent_breakpoint() {
        let mut state = SessionState::new();
        let dep = BreakpointLocation {
            source_path: "main.rs".to_string(),
            line: 10,
        };
        state.add_dependent_breakpoint("main.rs".to_string(), 20, None, None, dep.clone());

        assert_eq!(state.dependent_breakpoints.len(), 1);
        assert_eq!(state.dependent_breakpoints[0].line, 20);
        assert_eq!(state.dependent_breakpoints[0].activate_after, dep);
    }

    #[test]
    fn test_remove_dependent_breakpoint() {
        let mut state = SessionState::new();
        let dep = BreakpointLocation {
            source_path: "main.rs".to_string(),
            line: 10,
        };
        state.add_dependent_breakpoint("main.rs".to_string(), 20, None, None, dep);

        assert!(state.remove_dependent_breakpoint("main.rs", 20));
        assert!(state.dependent_breakpoints.is_empty());
        assert!(!state.remove_dependent_breakpoint("main.rs", 20));
    }

    #[test]
    fn test_record_breakpoint_hit() {
        let mut state = SessionState::new();
        state.record_breakpoint_hit("main.rs", 10);

        assert!(state.hit_locations.contains(&BreakpointLocation {
            source_path: "main.rs".to_string(),
            line: 10,
        }));
    }

    #[test]
    fn test_take_activated_dependents_none_ready() {
        let mut state = SessionState::new();
        let dep = BreakpointLocation {
            source_path: "main.rs".to_string(),
            line: 10,
        };
        state.add_dependent_breakpoint("main.rs".to_string(), 20, None, None, dep);

        let activated = state.take_activated_dependents();
        assert!(activated.is_empty());
        assert_eq!(state.dependent_breakpoints.len(), 1);
    }

    #[test]
    fn test_take_activated_dependents_one_ready() {
        let mut state = SessionState::new();
        let dep = BreakpointLocation {
            source_path: "main.rs".to_string(),
            line: 10,
        };
        state.add_dependent_breakpoint(
            "main.rs".to_string(), 20,
            Some("x > 5".to_string()), None,
            dep,
        );
        // A second dependent that is NOT ready
        let dep2 = BreakpointLocation {
            source_path: "other.rs".to_string(),
            line: 99,
        };
        state.add_dependent_breakpoint("other.rs".to_string(), 50, None, None, dep2);

        // Hit the first dependency
        state.record_breakpoint_hit("main.rs", 10);

        let activated = state.take_activated_dependents();
        assert_eq!(activated.len(), 1);
        assert_eq!(activated[0].source_path, "main.rs");
        assert_eq!(activated[0].line, 20);
        assert_eq!(activated[0].condition, Some("x > 5".to_string()));

        // The second one remains
        assert_eq!(state.dependent_breakpoints.len(), 1);
        assert_eq!(state.dependent_breakpoints[0].source_path, "other.rs");
    }

    #[test]
    fn test_take_activated_dependents_idempotent() {
        let mut state = SessionState::new();
        let dep = BreakpointLocation {
            source_path: "main.rs".to_string(),
            line: 10,
        };
        state.add_dependent_breakpoint("main.rs".to_string(), 20, None, None, dep);
        state.record_breakpoint_hit("main.rs", 10);

        let activated = state.take_activated_dependents();
        assert_eq!(activated.len(), 1);

        // Calling again returns nothing (the dependent was already taken)
        let activated2 = state.take_activated_dependents();
        assert!(activated2.is_empty());
    }
}
