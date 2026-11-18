#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    #[test]
    fn voice_settings_update_omitted_leaves_gate_unchanged() {
        // Unrelated settings push must not flip the gate (default-on stays on; kill-switch stays off until an explicit true/false)
        let mut app = make_app_with_agent("sess-1");
        app.apply_voice_mode_enabled(true);
        let omit = acp::ExtNotification::new(
            "x.ai/settings/update",
            std::sync::Arc::from(
                serde_json::value::to_raw_value(&serde_json::json!({ "sharing_enabled": true }))
                    .unwrap(),
            ),
        );
        let _ = handle_ext_notification(&omit, &mut app);
        assert!(app.voice_mode_enabled);

        app.apply_voice_mode_enabled(false);
        let _ = handle_ext_notification(&omit, &mut app);
        assert!(!app.voice_mode_enabled);
    }

    /// A user-owned mode blocks remote pushes from re-enabling default_yolo or rewriting the UI.
    #[test]
    fn permission_mode_user_claim_blocks_default_yolo_rearm() {
        let mut app = make_app_with_agent("sess-user-claim");
        app.auto_mode_gate = true;
        app.permission_mode_from_soft_default = false;
        app.current_ui.permission_mode = Some("ask".into());
        app.default_yolo = false;

        let apply_yolo = acp::ExtNotification::new(
            "x.ai/settings/update",
            serde_json::value::to_raw_value(&serde_json::json!({
                "permission_mode": "always-approve",
            }))
            .unwrap()
            .into(),
        );
        let _ = handle_ext_notification(&apply_yolo, &mut app);
        assert!(
            !app.default_yolo,
            "user-claimed mode must not re-arm default_yolo from remote always-approve"
        );
        assert_eq!(
            app.current_ui.permission_mode.as_deref(),
            Some("ask"),
            "user-claimed UI must not be rewritten by remote soft-default"
        );
        assert!(
            !app.permission_mode_from_soft_default,
            "user claim origin stays false"
        );
    }

    #[test]
    fn permission_mode_omitted_does_not_clear_soft_default() {
        let mut app = make_app_with_agent("sess-omit-pm");
        app.permission_mode_from_soft_default = true;
        app.current_ui.permission_mode = Some("auto".into());
        app.default_yolo = false;
        app.auto_mode_gate = true;

        let unrelated = acp::ExtNotification::new(
            "x.ai/settings/update",
            serde_json::value::to_raw_value(&serde_json::json!({
                "show_resolved_model": true,
            }))
            .unwrap()
            .into(),
        );
        let _ = handle_ext_notification(&unrelated, &mut app);
        assert_eq!(
            app.current_ui.permission_mode.as_deref(),
            Some("auto"),
            "omitted permission_mode must not clear soft-applied UI mode"
        );
        assert!(
            app.permission_mode_from_soft_default,
            "origin must stay SoftDefault when field is omitted"
        );
        assert!(
            !app.default_yolo,
            "omitted permission_mode must not recompute default_yolo"
        );
    }

    #[test]
    fn permission_mode_soft_default_applies_remote_always_approve() {
        let mut app = make_app_with_agent("sess-pm");
        app.auto_mode_gate = true;
        app.permission_mode_from_soft_default = true;
        app.current_ui.permission_mode = None;
        app.default_yolo = false;

        super::super::settings::apply_soft_default_permission_mode(
            &mut app,
            None,
            Some("always-approve"),
        );
        assert!(app.default_yolo, "remote always-approve must arm default_yolo");
        assert_eq!(
            app.current_ui.permission_mode.as_deref(),
            Some("always-approve"),
        );
        assert!(
            app.permission_mode_from_soft_default,
            "a soft re-arm must keep SoftDefault origin"
        );
        assert!(
            app.pending_effects.is_empty(),
            "a soft default must never be persisted to disk"
        );
    }

    #[test]
    fn permission_mode_null_clears_soft_always_approve() {
        let mut app = make_app_with_agent("sess-null-pm");
        app.auto_mode_gate = true;
        app.permission_mode_from_soft_default = true;
        app.current_ui.permission_mode = Some("always-approve".into());
        app.default_yolo = true;

        super::super::settings::apply_soft_default_permission_mode(&mut app, None, None);
        assert!(!app.default_yolo, "remote null must disarm a soft always-approve");
        assert_ne!(
            app.current_ui.permission_mode.as_deref(),
            Some("always-approve"),
            "the soft always-approve must be cleared from the UI mirror"
        );
        assert!(app.permission_mode_from_soft_default);
        assert!(
            app.pending_effects.is_empty(),
            "a soft default must never be persisted to disk"
        );
    }

    #[test]
    fn permission_mode_broken_config_stays_ask() {
        let mut app = make_app_with_agent("sess-broken-cfg");
        app.auto_mode_gate = true;
        app.permission_mode_from_soft_default = true;

        let fallback = super::super::settings::broken_config_ask_fallback();
        super::super::settings::apply_soft_default_permission_mode(
            &mut app,
            fallback.get("ui"),
            Some("always-approve"),
        );
        assert!(!app.default_yolo, "broken config must not arm always-approve");
        assert_eq!(app.current_ui.permission_mode.as_deref(), Some("ask"));
    }

    #[test]
    fn permission_mode_soft_default_respects_pin_and_gate() {
        let mut app = make_app_with_agent("sess-pin-pm");
        app.permission_mode_from_soft_default = true;
        app.yolo_policy_block = Some("pinned");
        app.default_yolo = false;
        super::super::settings::apply_soft_default_permission_mode(
            &mut app,
            None,
            Some("always-approve"),
        );
        assert!(!app.default_yolo, "policy pin must block a remote always-approve");
        assert_eq!(app.current_ui.permission_mode.as_deref(), Some("ask"));

        let mut app = make_app_with_agent("sess-gate-pm");
        app.permission_mode_from_soft_default = true;
        app.auto_mode_gate = false;
        super::super::settings::apply_soft_default_permission_mode(&mut app, None, Some("auto"));
        assert!(!app.default_yolo);
        assert_eq!(
            app.current_ui.permission_mode.as_deref(),
            Some("ask"),
            "gated-off Auto must display as Ask"
        );
    }
