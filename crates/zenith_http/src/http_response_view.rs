use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Window,
};
use ui::{
    Button, ButtonCommon, ButtonStyle, Color, Label, LabelCommon,
    LabelSize, v_flex, h_flex, prelude::*,
};
use crate::parser::HttpRequest;
use crate::response::HttpResponse;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseTab {
    Body,
    Headers,
    Raw,
    Request,
}

pub struct HttpResponseView {
    pub response: HttpResponse,
    pub request: HttpRequest,
    pub active_tab: ResponseTab,
    pub focus_handle: FocusHandle,
}

impl HttpResponseView {
    pub fn new(response: HttpResponse, request: HttpRequest, cx: &mut App) -> Self {
        Self {
            response,
            request,
            active_tab: ResponseTab::Body,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_active_tab(&mut self, tab: ResponseTab, cx: &mut Context<Self>) {
        self.active_tab = tab;
        cx.notify();
    }
}

impl Focusable for HttpResponseView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HttpResponseView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status_color = if self.response.is_success() {
            Color::Created
        } else if self.response.is_client_error() {
            Color::Warning
        } else {
            Color::Error
        };

        let status_bg = if self.response.is_success() {
            cx.theme().status().created_background
        } else if self.response.is_client_error() {
            cx.theme().status().warning_background
        } else {
            cx.theme().status().error_background
        };

        let method_color = match self.request.method.as_str() {
            "GET" => Color::Accent,
            "POST" => Color::Created,
            "PUT" | "PATCH" => Color::Warning,
            "DELETE" => Color::Error,
            _ => Color::Default,
        };

        let size_str = if self.response.size_bytes < 1024 {
            format!("{} B", self.response.size_bytes)
        } else {
            format!("{:.1} KB", self.response.size_bytes as f64 / 1024.0)
        };

        v_flex()
            .id("http-response-view")
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .track_focus(&self.focus_handle)
            .child(
                // Header bar
                h_flex()
                    .px_4()
                    .py_3()
                    .justify_between()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().elevated_surface_background)
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                h_flex()
                                    .px_2()
                                    .py_0p5()
                                    .bg(cx.theme().colors().element_selected)
                                    .rounded_sm()
                                    .child(
                                        Label::new(self.request.method.clone())
                                            .size(LabelSize::Small)
                                            .color(method_color),
                                    ),
                            )
                            .child(
                                Label::new(self.request.url.clone())
                                    .size(LabelSize::Small),
                            )
                            .child(
                                h_flex()
                                    .px_2()
                                    .py_0p5()
                                    .bg(status_bg)
                                    .rounded_sm()
                                    .child(
                                        Label::new(format!("{} {}", self.response.status, self.response.status_text))
                                            .size(LabelSize::Small)
                                            .color(status_color),
                                    ),
                            )
                            .child(
                                Label::new(format!("{}ms", self.response.duration_ms))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(size_str)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("copy-curl-btn", "Copy as cURL")
                                    .style(ButtonStyle::Subtle)
                                    .on_click({
                                        let curl = self.request.to_curl();
                                        move |_, _window, cx| {
                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(curl.clone()));
                                        }
                                    }),
                            )
                            .child(
                                Button::new("copy-body-btn", "Copy Body")
                                    .style(ButtonStyle::Subtle)
                                    .on_click({
                                        let body = self.response.body.clone();
                                        move |_, _window, cx| {
                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(body.clone()));
                                        }
                                    }),
                            ),
                    ),
            )
            .child(
                // Tab switcher bar
                h_flex()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().panel_background)
                    .child(
                        Button::new("tab-body", "Body")
                            .style(if self.active_tab == ResponseTab::Body {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.set_active_tab(ResponseTab::Body, cx);
                            })),
                    )
                    .child(
                        Button::new("tab-headers", "Headers")
                            .style(if self.active_tab == ResponseTab::Headers {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.set_active_tab(ResponseTab::Headers, cx);
                            })),
                    )
                    .child(
                        Button::new("tab-raw", "Raw")
                            .style(if self.active_tab == ResponseTab::Raw {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.set_active_tab(ResponseTab::Raw, cx);
                            })),
                    )
                    .child(
                        Button::new("tab-req", "Request Info")
                            .style(if self.active_tab == ResponseTab::Request {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.set_active_tab(ResponseTab::Request, cx);
                            })),
                    ),
            )
            .child(
                // Content area
                v_flex()
                    .id("http-response-content")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_4()
                    .children(match self.active_tab {
                        ResponseTab::Body => {
                            let formatted = self.response.format_body();
                            vec![
                                v_flex()
                                    .id("body-content")
                                    .child(
                                        Label::new(formatted)
                                            .size(LabelSize::Small),
                                    )
                                    .into_any_element(),
                            ]
                        }
                        ResponseTab::Headers => {
                            self.response
                                .headers
                                .iter()
                                .enumerate()
                                .map(|(ix, (k, v))| {
                                    h_flex()
                                        .id(format!("header-row-{ix}"))
                                        .py_1()
                                        .gap_2()
                                        .child(
                                            Label::new(format!("{k}:"))
                                                .size(LabelSize::Small)
                                                .color(Color::Accent),
                                        )
                                        .child(
                                            Label::new(v.clone())
                                                .size(LabelSize::Small),
                                        )
                                        .into_any_element()
                                })
                                .collect()
                        }
                        ResponseTab::Raw => {
                            let raw = self.response.to_raw_http();
                            vec![
                                v_flex()
                                    .id("raw-content")
                                    .child(
                                        Label::new(raw)
                                            .size(LabelSize::Small),
                                    )
                                    .into_any_element(),
                            ]
                        }
                        ResponseTab::Request => {
                            let curl = self.request.to_curl();
                            vec![
                                v_flex()
                                    .id("req-info")
                                    .gap_2()
                                    .child(
                                        Label::new("Equivalent cURL:")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(curl)
                                            .size(LabelSize::Small)
                                            .color(Color::Accent),
                                    )
                                    .into_any_element(),
                            ]
                        }
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    #[gpui::test]
    fn test_http_response_view_tab_switch(cx: &mut App) {
        let req = HttpRequest::new("POST", "https://api.example.com/v1/users", 1);
        let res = HttpResponse::new(200, "OK", vec![("Content-Type".into(), "application/json".into())], "{\"id\":1}", 45);

        let view = cx.new(|cx| HttpResponseView::new(res, req, cx));

        view.update(cx, |this, cx| {
            assert_eq!(this.active_tab, ResponseTab::Body);
            this.set_active_tab(ResponseTab::Headers, cx);
            assert_eq!(this.active_tab, ResponseTab::Headers);
            this.set_active_tab(ResponseTab::Raw, cx);
            assert_eq!(this.active_tab, ResponseTab::Raw);
            this.set_active_tab(ResponseTab::Request, cx);
            assert_eq!(this.active_tab, ResponseTab::Request);
        });
    }
}
