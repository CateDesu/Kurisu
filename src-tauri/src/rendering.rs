pub fn configure() {
    let enabled = enabled_for_launch(std::env::var("KURISU_DMABUF").ok().as_deref());
    // WebKit reads this before creating its first window.
    if !enabled {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

fn enabled_for_launch(override_value: Option<&str>) -> bool {
    override_value != Some("0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceleration_is_enabled_unless_explicitly_disabled() {
        assert!(enabled_for_launch(None));
        assert!(enabled_for_launch(Some("1")));
        assert!(enabled_for_launch(Some("invalid")));
        assert!(!enabled_for_launch(Some("0")));
    }
}
