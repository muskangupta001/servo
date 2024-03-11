/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;

use egui::{
    CentralPanel, Color32, Frame, InnerResponse, Key, Modifiers, PaintCallback, Pos2,
    TopBottomPanel, Vec2,
};
use egui_glow::CallbackFn;
use egui_winit::EventResponse;
use euclid::{Length, Point2D, Rect, Scale, Size2D};
use gleam::gl;
use glow::NativeFramebuffer;
use log::{info, trace, warn};
use servo::compositing::windowing::EmbedderEvent;
use servo::msg::constellation_msg::TraversalDirection;
use servo::rendering_context::RenderingContext;
use servo::servo_geometry::DeviceIndependentPixel;
use servo::servo_url::ServoUrl;
use servo::style_traits::DevicePixel;

use crate::egui_glue::EguiGlow;
use crate::events_loop::EventsLoop;
use crate::geometry::winit_position_to_euclid_point;
use crate::parser::location_bar_input_to_url;
use crate::webview::WebViewManager;
use crate::window_trait::WindowPortsMethods;

pub struct Minibrowser {
    pub context: EguiGlow,
    pub event_queue: RefCell<Vec<MinibrowserEvent>>,
    pub toolbar_height: Length<f32, DeviceIndependentPixel>,

    /// The framebuffer object name for the widget surface we should draw to, or None if our widget
    /// surface does not use a framebuffer object.
    widget_surface_fbo: Option<NativeFramebuffer>,

    last_update: Instant,
    last_mouse_position: Option<Point2D<f32, DeviceIndependentPixel>>,
    location: RefCell<String>,
}

pub enum MinibrowserEvent {
    /// Go button clicked.
    Go,
    Back,
    Forward,
}

impl Minibrowser {
    pub fn new(
        rendering_context: &RenderingContext,
        events_loop: &EventsLoop,
        window: &dyn WindowPortsMethods,
        initial_url: ServoUrl,
    ) -> Self {
        let gl = unsafe {
            glow::Context::from_loader_function(|s| rendering_context.get_proc_address(s))
        };

        // Adapted from https://github.com/emilk/egui/blob/9478e50d012c5138551c38cbee16b07bc1fcf283/crates/egui_glow/examples/pure_glow.rs
        let context = EguiGlow::new(events_loop.as_winit(), Arc::new(gl), None);
        context
            .egui_ctx
            .set_pixels_per_point(window.hidpi_factor().get());

        let widget_surface_fbo = match rendering_context.context_surface_info() {
            Ok(Some(info)) => NonZeroU32::new(info.framebuffer_object).map(NativeFramebuffer),
            Ok(None) => panic!("Failed to get widget surface info from surfman!"),
            Err(error) => panic!("Failed to get widget surface info from surfman! {error:?}"),
        };

        Self {
            context,
            event_queue: RefCell::new(vec![]),
            toolbar_height: Default::default(),
            widget_surface_fbo,
            last_update: Instant::now(),
            last_mouse_position: None,
            location: RefCell::new(initial_url.to_string()),
        }
    }

    /// Preprocess the given [winit::event::WindowEvent], returning unconsumed for mouse events in
    /// the Servo browser rect. This is needed because the CentralPanel we create for our webview
    /// would otherwise make egui report events in that area as consumed.
    pub fn on_event(&mut self, event: &winit::event::WindowEvent<'_>) -> EventResponse {
        let mut result = self.context.on_event(event);
        result.consumed &= match event {
            winit::event::WindowEvent::CursorMoved { position, .. } => {
                let scale = Scale::<_, DeviceIndependentPixel, _>::new(
                    self.context.egui_ctx.pixels_per_point(),
                );
                self.last_mouse_position =
                    Some(winit_position_to_euclid_point(*position).to_f32() / scale);
                self.last_mouse_position
                    .map_or(false, |p| self.is_in_browser_rect(p))
            },
            winit::event::WindowEvent::MouseWheel { .. } |
            winit::event::WindowEvent::MouseInput { .. } => self
                .last_mouse_position
                .map_or(false, |p| self.is_in_browser_rect(p)),
            _ => true,
        };
        result
    }

    /// Return true iff the given position is in the Servo browser rect.
    fn is_in_browser_rect(&self, position: Point2D<f32, DeviceIndependentPixel>) -> bool {
        position.y < self.toolbar_height.get()
    }

    /// Update the minibrowser, but don’t paint.
    /// If `servo_framebuffer_id` is given, set up a paint callback to blit its contents to our
    /// CentralPanel when [`Minibrowser::paint`] is called.
    pub fn update(
        &mut self,
        window: &winit::window::Window,
        webviews: &mut WebViewManager<dyn WindowPortsMethods>,
        servo_framebuffer_id: Option<gl::GLuint>,
        reason: &'static str,
    ) {
        let now = Instant::now();
        trace!(
            "{:?} since last update ({})",
            now - self.last_update,
            reason
        );
        let Self {
            context,
            event_queue,
            toolbar_height,
            widget_surface_fbo,
            last_update,
            location,
            ..
        } = self;
        let widget_fbo = *widget_surface_fbo;
        let _duration = context.run(window, |ctx| {
            let mut embedder_events = vec![];
            let focused_webview_id = webviews.focused_webview_id();
            let mut selected_tab = focused_webview_id;
            TopBottomPanel::top("tab bar").show(ctx, |ui| {
                ui.allocate_ui_with_layout(
                    ui.available_size(),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        if ui.button("❌").clicked() {
                            if let Some(webview_id) = focused_webview_id {
                                embedder_events.push(EmbedderEvent::CloseWebView(webview_id));
                            }
                        }

                        let mut clicked_tab_webview_id = None;
                        let mut middle_clicked_tab_webview_id = None;
                        for (&webview_id, _) in webviews.creation_order() {
                            let text = format!("{:?}", webview_id.0);
                            let tab =
                                ui.selectable_value(&mut selected_tab, Some(webview_id), text);
                            if tab.clicked() {
                                info!("Clicked tab {webview_id}");
                                clicked_tab_webview_id = Some(webview_id);
                            }
                            if tab.clicked_by(egui::PointerButton::Middle) {
                                info!("Middle-clicked tab {webview_id}");
                                middle_clicked_tab_webview_id = Some(webview_id);
                            }
                        }
                        if let Some(clicked_tab_webview_id) = clicked_tab_webview_id {
                            // Blur then raise then focus, to avoid clickjacking.
                            embedder_events.push(EmbedderEvent::BlurWebView);
                            embedder_events.push(EmbedderEvent::RaiseWebViewToTop(
                                clicked_tab_webview_id,
                                true,
                            ));
                            embedder_events
                                .push(EmbedderEvent::FocusWebView(clicked_tab_webview_id));
                        }
                        if let Some(webview_id) = middle_clicked_tab_webview_id {
                            embedder_events.push(EmbedderEvent::CloseWebView(webview_id));
                        }
                    },
                );
            });

            let mut location_dirty = false;
            let mut location = location.borrow_mut();
            if let Some(webview) = webviews.focused_webview_mut() {
                location_dirty = webview.location_dirty;
                if webview.location != *location {
                    *location = webview.location.clone();
                }
            }

            let InnerResponse { inner: height, .. } =
                TopBottomPanel::top("toolbar").show(ctx, |ui| {
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            if ui.button("back").clicked() {
                                event_queue.borrow_mut().push(MinibrowserEvent::Back);
                            }
                            if ui.button("forward").clicked() {
                                event_queue.borrow_mut().push(MinibrowserEvent::Forward);
                            }
                            ui.allocate_ui_with_layout(
                                ui.available_size(),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("go").clicked() {
                                        event_queue.borrow_mut().push(MinibrowserEvent::Go);
                                        location_dirty = false;
                                    }

                                    let location_field = egui::TextEdit::singleline(&mut *location)
                                        .text_color_opt(location_dirty.then_some(Color32::YELLOW));
                                    let location_field =
                                        ui.add_sized(ui.available_size(), location_field);

                                    if location_field.changed() {
                                        location_dirty = true;
                                    }
                                    if ui.input(|i| {
                                        i.clone().consume_key(Modifiers::COMMAND, Key::L)
                                    }) {
                                        location_field.request_focus();
                                    }
                                    if location_field.lost_focus() &&
                                        ui.input(|i| i.clone().key_pressed(Key::Enter))
                                    {
                                        event_queue.borrow_mut().push(MinibrowserEvent::Go);
                                        location_dirty = false;
                                    }
                                },
                            );
                        },
                    );
                    ui.cursor().min.y
                });

            *toolbar_height = Length::new(height);
            if let Some(webview) = webviews.focused_webview_mut() {
                webview.location_dirty = location_dirty;
                if *location != webview.location {
                    webview.location = location.clone();
                }
            }

            let Some(focused_webview_id) = focused_webview_id else {
                return;
            };
            let Some(focused_webview) = webviews.get_mut(focused_webview_id) else {
                return;
            };
            let scale =
                Scale::<_, DeviceIndependentPixel, DevicePixel>::new(ctx.pixels_per_point());
            CentralPanel::default()
                .frame(Frame::none())
                .show(ctx, |ui| {
                    let Pos2 { x, y } = ui.cursor().min;
                    let Vec2 {
                        x: width,
                        y: height,
                    } = ui.available_size();
                    let rect = Rect::new(Point2D::new(x, y), Size2D::new(width, height)) * scale;
                    if rect != focused_webview.rect {
                        focused_webview.rect = rect;
                        embedder_events
                            .push(EmbedderEvent::MoveResizeWebView(focused_webview_id, rect));
                    }
                    let min = ui.cursor().min;
                    let size = ui.available_size();
                    let rect = egui::Rect::from_min_size(min, size);
                    ui.allocate_space(size);

                    let Some(servo_fbo) = servo_framebuffer_id else {
                        return;
                    };
                    ui.painter().add(PaintCallback {
                        rect,
                        callback: Arc::new(CallbackFn::new(move |info, painter| {
                            use glow::HasContext as _;
                            let clip = info.viewport_in_pixels();
                            let x = clip.left_px as gl::GLint;
                            let y = clip.from_bottom_px as gl::GLint;
                            let width = clip.width_px as gl::GLsizei;
                            let height = clip.height_px as gl::GLsizei;
                            unsafe {
                                painter.gl().clear_color(0.0, 0.0, 0.0, 0.0);
                                painter.gl().scissor(x, y, width, height);
                                painter.gl().enable(gl::SCISSOR_TEST);
                                painter.gl().clear(gl::COLOR_BUFFER_BIT);
                                painter.gl().disable(gl::SCISSOR_TEST);

                                let servo_fbo = NonZeroU32::new(servo_fbo).map(NativeFramebuffer);
                                painter
                                    .gl()
                                    .bind_framebuffer(gl::READ_FRAMEBUFFER, servo_fbo);
                                painter
                                    .gl()
                                    .bind_framebuffer(gl::DRAW_FRAMEBUFFER, widget_fbo);
                                painter.gl().blit_framebuffer(
                                    x,
                                    y,
                                    x + width,
                                    y + height,
                                    x,
                                    y,
                                    x + width,
                                    y + height,
                                    gl::COLOR_BUFFER_BIT,
                                    gl::NEAREST,
                                );
                                painter.gl().bind_framebuffer(gl::FRAMEBUFFER, widget_fbo);
                            }
                        })),
                    });
                });

            if !embedder_events.is_empty() {
                for event in embedder_events.iter() {
                    info!("Queueing embedder event: {event:?}");
                }
                webviews.handle_window_events(embedder_events);
            }

            *last_update = now;
        });
    }

    /// Paint the minibrowser, as of the last update.
    pub fn paint(&mut self, window: &winit::window::Window) {
        unsafe {
            use glow::HasContext as _;
            self.context
                .painter
                .gl()
                .bind_framebuffer(gl::FRAMEBUFFER, self.widget_surface_fbo);
        }
        self.context.paint(window);
    }

    /// Takes any outstanding events from the [Minibrowser], converting them to [EmbedderEvent] and
    /// routing those to the App event queue.
    pub fn queue_embedder_events_for_minibrowser_events(
        &self,
        browser: &WebViewManager<dyn WindowPortsMethods>,
        app_event_queue: &mut Vec<EmbedderEvent>,
    ) {
        for event in self.event_queue.borrow_mut().drain(..) {
            match event {
                MinibrowserEvent::Go => {
                    let browser_id = browser.webview_id().unwrap();
                    let location = self.location.borrow();
                    if let Some(url) = location_bar_input_to_url(&location.clone()) {
                        app_event_queue.push(EmbedderEvent::LoadUrl(browser_id, url));
                    } else {
                        warn!("failed to parse location");
                        break;
                    }
                },
                MinibrowserEvent::Back => {
                    let browser_id = browser.webview_id().unwrap();
                    app_event_queue.push(EmbedderEvent::Navigation(
                        browser_id,
                        TraversalDirection::Back(1),
                    ));
                },
                MinibrowserEvent::Forward => {
                    let browser_id = browser.webview_id().unwrap();
                    app_event_queue.push(EmbedderEvent::Navigation(
                        browser_id,
                        TraversalDirection::Forward(1),
                    ));
                },
            }
        }
    }

    /// Updates the location field from the given [WebViewManager], unless the the user has started
    /// editing it without clicking Go, returning true iff it has changed (needing an egui update).
    pub fn update_location_in_toolbar(
        &mut self,
        webviews: &mut WebViewManager<dyn WindowPortsMethods>,
        focused_webview_changed: bool,
    ) -> bool {
        // If no webview is focused, just return true iff the focus changed.
        let Some(webview) = webviews.focused_webview_mut() else {
            return dbg!(focused_webview_changed);
        };

        // User edited without clicking Go?
        if dbg!(webview.location_dirty) {
            return false;
        }

        let new_location = webview.current_url_string.as_deref().unwrap_or("");
        if dbg!(new_location) != dbg!(&webview.location) {
            webview.location = new_location.to_owned();
            return true;
        }

        false
    }
}
