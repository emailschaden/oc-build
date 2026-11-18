use super::*;
use serde::Deserialize;

/// Handle `x.ai/models/update`: the model list changed (etag-triggered refresh).
pub(super) fn handle_models_update(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    if let Ok(model_state) = serde_json::from_str::<acp::SessionModelState>(notif.params.get()) {
        use crate::acp::model_state::ModelState;
        let new_models = ModelState::from(Some(model_state));
        tracing::info!(
            count = new_models.available.len(),
            "models updated via x.ai/models/update"
        );

        app.models.update_catalog(new_models.available.clone());
        let stale = app
            .models
            .current
            .as_ref()
            .is_none_or(|id| !app.models.available.contains_key(id));
        if stale && let Some(id) = new_models.current {
            app.models.set_current(id, None);
        }

        for agent in app.agents.values_mut() {
            if let Some(ref current) = agent.session.models.current
                && !new_models.available.contains_key(current)
            {
                tracing::debug!(
                    current_model = %current.0,
                    available_count = new_models.available.len(),
                    "models update dropped this session's model from the catalog; keeping it displayed"
                );
            }
            agent
                .session
                .models
                .update_catalog(new_models.available.clone());
            agent.refresh_context_total();
        }
        true
    } else {
        tracing::warn!("Failed to parse x.ai/models/update");
        false
    }
}

/// Failed config load: pin explicit ask so a remote push cannot escalate.
pub(super) fn broken_config_ask_fallback() -> toml::Value {
    let mut ui = toml::value::Table::new();
    ui.insert("permission_mode".into(), toml::Value::String("ask".into()));
    let mut root = toml::value::Table::new();
    root.insert("ui".into(), toml::Value::Table(ui));
    toml::Value::Table(root)
}

/// Set up the next `/new` from a pushed `permission_mode`.
/// Live sessions stay untouched; nothing is persisted.
pub(super) fn apply_soft_default_permission_mode(
    app: &mut AppView,
    effective_ui: Option<&toml::Value>,
    remote: Option<&str>,
) {
    let mode = xai_grok_shell::util::config::selected_permission_mode(effective_ui, remote)
        .unwrap_or_else(xai_grok_shell::util::config::default_interactive_permission_mode);
    app.default_yolo = mode.is_always_approve() && app.yolo_policy_block.is_none();
    let auto = mode.is_auto() && app.auto_mode_gate && !app.default_yolo;
    app.current_ui.permission_mode = Some(if auto {
        "auto".to_string()
    } else if app.default_yolo {
        "always-approve".to_string()
    } else {
        xai_grok_shell::util::config::resolved_display_permission_mode(effective_ui, remote)
            .to_string()
    });
}

/// Tell live sessions to leave Auto on the mid-session kill-switch.
/// `yolo_mode` is deliberately OMITTED: the agent skips the yolo branch when the key is absent.
/// A sibling tab's always-approve is thus preserved; only auto is cleared.
pub(super) fn notify_sessions_leave_auto(app: &AppView, session_ids: &[acp::SessionId]) {
    for session_id in session_ids {
        let params = serde_json::json!({
            "sessionId": session_id,
            "auto_mode": false,
            "permission_mode": "ask",
        });
        let notification = acp::ExtNotification::new(
            "x.ai/yolo_mode_changed",
            serde_json::value::to_raw_value(&params)
                .expect("serialize yolo_mode_changed params")
                .into(),
        );

        let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
        let args = xai_acp_lib::AcpArgs {
            request: notification,
            response_tx,
        };
        let _ = app.acp_tx.send(args.into());
    }
}

pub(super) fn handle_announcements_update(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(parsed) =
        serde_json::from_str::<xai_grok_announcements::AnnouncementsRefreshed>(notif.params.get())
    else {
        return false;
    };

    if parsed.r#gen <= app.announcements_last_gen {
        return false;
    }

    // Re-merge config layers like startup does: the push carries the remote list only
    // A wholesale replace would drop requirements/user/managed announcements and let the prune erase their persisted hide keys
    // The settings handler performs the same disk reads; pushes are rare
    let requirements = xai_grok_shell::config::load_merged_requirements();
    let user_config = xai_grok_shell::config::load_from_disk().ok();
    let managed_config = xai_grok_shell::config::load_managed_config().ok();
    apply_announcements_update(
        app,
        parsed.r#gen,
        &parsed.announcements,
        requirements.as_ref(),
        user_config.as_ref(),
        managed_config.as_ref(),
    );
    true
}

/// Apply half of [`handle_announcements_update`], with config layers injected so the merge/prune behavior is unit-testable without disk state.
/// `resolve_announcements` honors `GROK_ANNOUNCEMENTS_OVERRIDE` first, so a backend push can't reintroduce announcements when the override is set.
pub(super) fn apply_announcements_update(
    app: &mut AppView,
    next_gen: u64,
    remote: &[xai_grok_announcements::RemoteAnnouncement],
    requirements: Option<&toml::Value>,
    user_config: Option<&toml::Value>,
    managed_config: Option<&toml::Value>,
) {
    let merged = xai_grok_shell::util::config::resolve_announcements(
        requirements,
        user_config,
        managed_config,
        Some(remote),
    );
    let announcements = xai_grok_announcements::filter_expired(merged);

    app.announcement = match app.announcement.as_ref() {
        Some(current) => announcements
            .iter()
            .find(|a| *a == current)
            .cloned()
            .or_else(|| pick_random_announcement(&announcements)),
        None => pick_random_announcement(&announcements),
    };
    app.active_announcements = announcements;
    app.announcements_last_gen = next_gen;
    // Opportunistic per-ID prune on a real update (never per frame) so the hidden set cannot grow unboundedly.
    if xai_grok_announcements::prune_hidden_announcement_ids(
        &mut app.hidden_announcement_ids,
        &app.active_announcements,
    ) {
        app.pending_effects
            .push(Effect::PersistAnnouncementsHidden {
                hidden_ids: app.hidden_announcement_ids.clone(),
            });
    }
    app.sync_session_announcement_slash_gate();
}

pub(super) fn pick_random_announcement(
    announcements: &[xai_grok_announcements::RemoteAnnouncement],
) -> Option<xai_grok_announcements::RemoteAnnouncement> {
    if announcements.is_empty() {
        return None;
    }
    use rand::Rng;
    let idx = rand::rng().random_range(0..announcements.len());
    announcements.get(idx).cloned()
}
