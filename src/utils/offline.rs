//! Process-wide offline mode, set once in `main` from `--offline` or
//! `AVOCADO_OFFLINE=1`. Every reader goes through `is_offline()`.

use std::sync::atomic::{AtomicBool, Ordering};

static OFFLINE: AtomicBool = AtomicBool::new(false);

pub fn set_offline(value: bool) {
    OFFLINE.store(value, Ordering::SeqCst);
}

pub fn is_offline() -> bool {
    OFFLINE.load(Ordering::SeqCst)
}

/// Offline when the flag is passed or `AVOCADO_OFFLINE` is `1`, `true` or `yes`
/// (the spellings `AVOCADO_REPO_INSECURE` accepts).
pub fn resolve(flag: bool) -> bool {
    flag || std::env::var("AVOCADO_OFFLINE")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial(offline)]
    fn offline_flag_sets_state() {
        std::env::remove_var("AVOCADO_OFFLINE");
        set_offline(false);
        set_offline(resolve(true));
        assert!(is_offline());
        set_offline(resolve(false));
        assert!(!is_offline());
    }

    #[test]
    #[serial_test::serial(offline)]
    fn offline_env_accepts_the_usual_truthy_spellings() {
        for (v, want) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("0", false),
            ("false", false),
            ("", false),
            ("2", false),
        ] {
            std::env::set_var("AVOCADO_OFFLINE", v);
            assert_eq!(resolve(false), want, "AVOCADO_OFFLINE={v:?}");
        }
        std::env::remove_var("AVOCADO_OFFLINE");
        assert!(!resolve(false));
    }

    #[test]
    #[serial_test::serial(offline)]
    fn offline_env_sets_state() {
        set_offline(false);
        std::env::set_var("AVOCADO_OFFLINE", "1");
        set_offline(resolve(false));
        std::env::remove_var("AVOCADO_OFFLINE");
        assert!(is_offline());
        set_offline(resolve(false));
        assert!(!is_offline());
    }
}
