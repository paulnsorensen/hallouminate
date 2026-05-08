pub struct FenceTracker {
    in_fence: bool,
}

impl FenceTracker {
    pub fn new() -> Self {
        Self { in_fence: false }
    }

    pub fn skip_line(&mut self, line: &str) -> bool {
        if line.trim_start().starts_with("```") {
            self.in_fence = !self.in_fence;
            return true;
        }
        self.in_fence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_tracker_skips_delimiters_and_inner_content() {
        let lines = ["before", "```rust", "fn main() {}", "```", "after"];
        let mut fence = FenceTracker::new();
        let kept: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|line| !fence.skip_line(line))
            .collect();
        assert_eq!(kept, vec!["before", "after"]);
    }

    #[test]
    fn fence_tracker_handles_unclosed_fence() {
        let lines = ["before", "```rust", "fn main() {}"];
        let mut fence = FenceTracker::new();
        let kept: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|line| !fence.skip_line(line))
            .collect();
        assert_eq!(kept, vec!["before"]);
    }

    #[test]
    fn fence_tracker_treats_indented_fence_as_delimiter() {
        let lines = ["    ```", "inside", "    ```", "outside"];
        let mut fence = FenceTracker::new();
        let kept: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|line| !fence.skip_line(line))
            .collect();
        assert_eq!(kept, vec!["outside"]);
    }
}
