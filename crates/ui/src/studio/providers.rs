//! Studio provider settings: connect, validate, and remove image accounts.

use gpui::{
    AnyElement, Context, Entity, Render, SharedString, Subscription, Task, Window, div, prelude::*,
    px,
};
use zeron_proto::{ListStudioProvidersResponse, ProviderValidationState, StudioProviderConnection};
use zeron_rpc::methods;

use crate::icons;
use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::text_input::TextInput;
use crate::theme::Theme;

pub struct ProvidersPage {
    state: Entity<AppState>,
    providers: Vec<StudioProviderConnection>,
    secret: Entity<TextInput>,
    loading: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    _observe: Subscription,
}

impl ProvidersPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let secret = cx.new(|cx| TextInput::new("Provider API key", cx));
        let mut page = Self {
            state,
            providers: Vec::new(),
            secret,
            loading: false,
            error: None,
            task: None,
            _observe: observe,
        };
        page.load(cx);
        page
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            return;
        };
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_STUDIO_PROVIDERS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.loading = false;
                match result.and_then(|value| {
                    serde_json::from_value::<ListStudioProvidersResponse>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(value) => {
                        page.providers = value.providers;
                        page.revalidate_configured(cx);
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn revalidate_configured(&mut self, cx: &mut Context<Self>) {
        let ids = self
            .providers
            .iter()
            .filter(|provider| {
                provider.configured && provider.validation_state != ProviderValidationState::Valid
            })
            .map(|provider| provider.provider_id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task =
            Some(cx.spawn(async move |this, cx| {
                for provider_id in ids {
                    let result = engine
                        .client()
                        .call(
                            methods::VALIDATE_STUDIO_PROVIDER,
                            serde_json::json!({ "providerId": provider_id }),
                        )
                        .await;
                    let stop = this
                        .update(cx, |page, cx| {
                            match result.and_then(|value| {
                                serde_json::from_value::<StudioProviderConnection>(value)
                                    .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                            }) {
                                Ok(connection) => {
                                    if let Some(slot) = page.providers.iter_mut().find(|provider| {
                                        provider.provider_id == connection.provider_id
                                    }) {
                                        *slot = connection;
                                    }
                                }
                                Err(error) => page.error = Some(error.to_string().into()),
                            }
                            cx.notify();
                        })
                        .is_err();
                    if stop {
                        break;
                    }
                }
            }));
    }

    fn save(&mut self, provider_id: zeron_studio::ProviderId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let secret = self.secret.read(cx).text().trim().to_owned();
        if secret.is_empty() {
            return;
        }
        let display_label = self
            .providers
            .iter()
            .find(|provider| provider.provider_id == provider_id)
            .map(|provider| provider.display_label.clone())
            .unwrap_or_else(|| "Image provider".to_owned());
        self.error = None;
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                engine.client().call(methods::SET_STUDIO_PROVIDER_CREDENTIAL, serde_json::json!({ "providerId": provider_id, "displayLabel": display_label, "secret": secret })).await?;
                engine.client().call(methods::VALIDATE_STUDIO_PROVIDER, serde_json::json!({ "providerId": provider_id })).await
            }.await;
            this.update(cx, |page, cx| { page.loading = false; match result.and_then(|value| serde_json::from_value::<StudioProviderConnection>(value).map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))) { Ok(connection) => { if let Some(slot) = page.providers.iter_mut().find(|provider| provider.provider_id == connection.provider_id) { *slot = connection; } page.secret.update(cx, |input, cx| input.set_text("", cx)); }, Err(error) => page.error = Some(error.to_string().into()) } cx.notify(); }).ok();
        }));
    }

    fn set_safe_mode(
        &mut self,
        provider_id: zeron_studio::ProviderId,
        safe_mode: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.error = None;
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SET_STUDIO_PROVIDER_PREFERENCES,
                    serde_json::json!({ "providerId": provider_id, "safeMode": safe_mode }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.loading = false;
                match result.and_then(|value| {
                    serde_json::from_value::<StudioProviderConnection>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(connection) => {
                        if let Some(slot) = page
                            .providers
                            .iter_mut()
                            .find(|provider| provider.provider_id == connection.provider_id)
                        {
                            *slot = connection;
                        }
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn remove(&mut self, provider_id: zeron_studio::ProviderId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.error = None;
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_STUDIO_PROVIDER_CREDENTIAL,
                    serde_json::json!({ "providerId": provider_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.loading = false;
                match result {
                    Ok(_) => page.load(cx),
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_provider_section(
        &mut self,
        provider: StudioProviderConnection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = provider.provider_id.clone();
        let remove_id = provider.provider_id.clone();
        let toggle_id = provider.provider_id.clone();
        let configured = provider.configured;
        let safe_mode = provider.safe_mode;
        let venice = provider.provider_id.as_str() == "venice";
        let validation_message = provider
            .validation_message
            .clone()
            .filter(|_| provider.validation_state != ProviderValidationState::Valid);
        let label = if provider.display_label.eq_ignore_ascii_case("venice") {
            "Venice".to_owned()
        } else {
            provider.display_label.clone()
        };
        let (status, status_badge) = match provider.validation_state {
            ProviderValidationState::Valid => (
                "Connected",
                widgets::badge_active(theme, "Connected").into_any_element(),
            ),
            ProviderValidationState::Invalid => (
                "Invalid key",
                widgets::badge_danger(theme, "Invalid key").into_any_element(),
            ),
            ProviderValidationState::Unavailable => (
                "Unavailable",
                widgets::badge_warning(theme, "Unavailable").into_any_element(),
            ),
            ProviderValidationState::NotValidated if configured => (
                "Not validated",
                widgets::badge_warning(theme, "Not validated").into_any_element(),
            ),
            _ => (
                "Not connected",
                widgets::badge(theme, "Not connected").into_any_element(),
            ),
        };
        let save_label = if self.loading {
            "Working…"
        } else if configured {
            "Save and validate"
        } else {
            "Add key"
        };

        let mut card =
            widgets::section_card(theme).mt(px(8.0)).child(
                widgets::card_row(theme, true)
                    .child(widgets::row_tile(theme, icons::KEY_MINIMALISTIC))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "API key"))
                            .child(widgets::meta_line(
                                theme,
                                vec![div().child(SharedString::from(status)).into_any_element()],
                            )),
                    )
                    .child(status_badge)
                    .when(configured, |row| {
                        row.child(
                            widgets::ghost_action(theme)
                                .id(SharedString::from(format!(
                                    "provider-remove-{}",
                                    remove_id.as_str()
                                )))
                                .hover(|style| widgets::ghost_hover(theme, style))
                                .on_click(cx.listener(move |page, _, _, cx| {
                                    page.remove(remove_id.clone(), cx)
                                }))
                                .child(
                                    icons::icon(icons::TRASH_BIN_MINIMALISTIC)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Remove")),
                        )
                    }),
            );
        card = card.child(
            div()
                .px(px(20.0))
                .pb(px(14.0))
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(popover::dialog_field(
                    self.secret.clone().into_any_element(),
                ))
                .child(
                    div().flex().justify_end().child(
                        popover::btn_primary(theme, save_label)
                            .id(SharedString::from(format!("provider-save-{}", id.as_str())))
                            .when(self.loading, |el| el.opacity(0.5))
                            .on_click(cx.listener(move |page, _, _, cx| page.save(id.clone(), cx))),
                    ),
                ),
        );
        if configured && venice {
            card = card.child(
                widgets::card_row(theme, false)
                    .child(widgets::row_tile(theme, icons::TUNING))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "Safe mode"))
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    div()
                                        .child(SharedString::from(
                                            "Blur images Venice classifies as adult content.",
                                        ))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(theme, safe_mode)
                            .id(SharedString::from(format!(
                                "provider-safe-mode-{}",
                                toggle_id.as_str()
                            )))
                            .cursor_pointer()
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.set_safe_mode(toggle_id.clone(), !safe_mode, cx)
                            })),
                    ),
            );
        }

        div()
            .mt(px(24.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_none()
                            .size(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                icons::icon(icons::KEY_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(14.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(label)),
                    ),
            )
            .when_some(validation_message, |section, message| {
                section.child(widgets::warning_strip(theme, message))
            })
            .child(card)
            .into_any_element()
    }
}

impl Render for ProvidersPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let configured_count = {
            let count = self
                .providers
                .iter()
                .filter(|provider| provider.configured)
                .count();
            (count > 0).then_some(count)
        };
        let sections: Vec<AnyElement> = if self.loading && self.providers.is_empty() {
            vec![
                widgets::section_card(&theme)
                    .p(px(16.0))
                    .child(popover::skeleton_rows(
                        "providers-skeleton",
                        &theme,
                        2,
                        cx.entity_id(),
                        cx,
                    ))
                    .into_any_element(),
            ]
        } else {
            self.providers
                .clone()
                .into_iter()
                .map(|provider| self.render_provider_section(provider, &theme, cx))
                .collect()
        };

        div()
            .id("providers-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Providers", configured_count))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Image-generation accounts on this device. Keys stay in the platform \
                         secret store and are never synced.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(&theme, message)
                                .id("providers-action-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .children(sections)
                    .child(
                        div()
                            .mt(px(24.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .line_height(px(19.0))
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(
                                "Safe mode is a Venice setting. When it is on, images Venice \
                                 classifies as adult content come back blurred. It stays off \
                                 unless you turn it on.",
                            )),
                    ),
            )
    }
}
