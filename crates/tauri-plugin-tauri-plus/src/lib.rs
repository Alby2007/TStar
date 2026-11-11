//! Tauri plugin: supervisor, IPC bridge and permission enforcement for the
//! managed Python runtime. Stub until M0/M1 (see PROJECT_BRIEF.md section 7).

/// Returns the crate name, so dependent code and tests have something to link
/// against before the real plugin lands.
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_is_set() {
        assert_eq!(crate_name(), "tauri-plugin-tauri-plus");
    }
}
