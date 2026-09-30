//! A `wry` webview hosted in a window-composition surface (macOS).
//!
//! `Window::enable_window_composition` (longbridge/gpui-fast#30) layers a
//! transparent GPUI overlay above native views: the `WKWebView` is reparented
//! into a composition surface's container, so deferred content (dialogs,
//! sheets, tooltips, menus, notifications) paints above the page instead of
//! being occluded by it. When composition setup fails the webview stays a
//! direct child of the window, keeping the old stacking behavior.

use std::rc::Rc;

use gpui::{
    App, Bounds, Context, DispatchPhase, Element, ElementId, FocusHandle, Focusable,
    GlobalElementId, InteractiveElement, IntoElement, LayoutId, MouseDownEvent, ParentElement,
    Pixels, Render, Size, Style, Styled as _, Window, WindowCompositionSurface, div,
};
use gpui_component::ActiveTheme as _;
use wry::{
    Rect, dpi,
    dpi::{LogicalPosition, LogicalSize},
};

/// A `wry` webview whose `WKWebView` sits in a GPUI composition surface.
///
/// Mirrors the `gpui_wry::WebView` API surface the browser view uses. On drop
/// the surface's container view leaves the window, taking the native view
/// with it.
pub struct ComposedWebView {
    focus_handle: FocusHandle,
    /// Declared before `surface`: wry detaches the `WKWebView` on drop, so
    /// the native view is gone before the container view it lived in.
    webview: Rc<wry::WebView>,
    /// The composition surface hosting the webview; `None` in the
    /// direct-child fallback.
    surface: Option<WindowCompositionSurface>,
}

impl Drop for ComposedWebView {
    fn drop(&mut self) {
        self.hide();
    }
}

impl ComposedWebView {
    /// Reparents `webview`'s native view into a fresh composition surface.
    /// On any failure the webview stays a direct child of the window.
    pub fn new(webview: wry::WebView, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let surface = Self::attach_surface(&webview, window, cx);
        if surface.is_none() {
            // Direct-child fallback: start with an empty frame like gpui-wry.
            let _ = webview.set_bounds(Rect::default());
        }
        Self {
            focus_handle: cx.focus_handle(),
            webview: Rc::new(webview),
            surface,
        }
    }

    fn attach_surface(
        webview: &wry::WebView,
        window: &Window,
        cx: &App,
    ) -> Option<WindowCompositionSurface> {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        use wry::WebViewExtMacOS as _;

        let surface = match window
            .enable_window_composition()
            .and_then(|composition| composition.create_native_surface())
        {
            Ok(surface) => surface,
            Err(error) => {
                tracing::warn!(
                    "window composition unavailable; webview stays a direct child: {error:#}"
                );
                return None;
            }
        };
        let handle = match surface
            .platform_surface()
            .and_then(|platform| platform.platform_handle())
        {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!("composition surface has no platform handle: {error:#}");
                return None;
            }
        };
        let container = match handle.downcast::<usize>() {
            Ok(container) => *container as *mut AnyObject,
            Err(_) => {
                tracing::warn!("composition surface did not provide an AppKit view");
                return None;
            }
        };
        if container.is_null() {
            tracing::warn!("composition surface returned a null AppKit view");
            return None;
        }

        unsafe {
            let wk: *mut AnyObject = &*webview.webview() as *const _ as *mut AnyObject;
            // wry keeps the window's own view for `focus_parent`; only the
            // WKWebView moves into the container.
            let _: () = msg_send![wk, removeFromSuperview];
            let _: () = msg_send![container, addSubview: wk];
            // NSViewWidthSizable | NSViewHeightSizable: the container's frame
            // is authoritative; the webview fills it on every resize.
            let _: () = msg_send![wk, setAutoresizingMask: 18usize];
            // Clip the page to the same radius the GPUI frame draws.
            let layer: *mut AnyObject = msg_send![container, layer];
            if !layer.is_null() {
                let radius: f64 = f32::from(cx.theme().radius) as f64;
                let _: () = msg_send![layer, setMasksToBounds: true];
                let _: () = msg_send![layer, setCornerRadius: radius];
            }
        }
        Some(surface)
    }

    /// Show the webview. Callers must `cx.notify()` the entity afterwards so
    /// the element re-paints and re-applies the surface bounds; retained
    /// views otherwise replay the frame painted while it was hidden.
    pub fn show(&mut self) {
        let _ = self.webview.set_visible(true);
        if let Some(surface) = &self.surface {
            let _ = surface
                .platform_surface()
                .and_then(|platform| platform.set_visible(true));
        }
    }

    /// Hide the webview, returning keyboard focus to the window first. See
    /// [`Self::show`] for the required `cx.notify()`.
    pub fn hide(&mut self) {
        let _ = self.webview.focus_parent();
        let _ = self.webview.set_visible(false);
        if let Some(surface) = &self.surface {
            let _ = surface
                .platform_surface()
                .and_then(|platform| platform.set_visible(false));
        }
    }

    /// Go back in the webview history.
    pub fn back(&self) -> wry::Result<()> {
        self.webview.evaluate_script("history.back();")
    }

    /// Load a URL in the webview.
    pub fn load_url(&mut self, url: &str) {
        let _ = self.webview.load_url(url);
    }

    /// Get the raw wry webview.
    pub fn raw(&self) -> &wry::WebView {
        &self.webview
    }
}

impl Focusable for ComposedWebView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ComposedWebView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(ComposedWebViewElement {
                view: self.webview.clone(),
                surface: self.surface.clone(),
            })
    }
}

/// Element that keeps the composition surface (or, in the fallback, the
/// webview itself) glued to its layout bounds.
pub struct ComposedWebViewElement {
    view: Rc<wry::WebView>,
    surface: Option<WindowCompositionSurface>,
}

impl IntoElement for ComposedWebViewElement {
    type Element = ComposedWebViewElement;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ComposedWebViewElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: Size::full(),
            flex_shrink: 1.,
            ..Default::default()
        };
        let id = window.request_layout(style, [], cx);
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        if let Some(surface) = &self.surface {
            let device_bounds = bounds.to_device_pixels(window.scale_factor());
            if let Err(error) = surface
                .platform_surface()
                .and_then(|platform| platform.set_bounds(device_bounds))
            {
                tracing::warn!("failed to update webview surface bounds: {error:#}");
            }
            // The surface carries the window-space offset; inside the
            // container the webview fills it from the origin.
            let _ = self.view.set_bounds(Rect {
                size: dpi::Size::Logical(LogicalSize {
                    width: bounds.size.width.into(),
                    height: bounds.size.height.into(),
                }),
                position: dpi::Position::Logical(LogicalPosition::new(0., 0.)),
            });
        } else {
            let _ = self.view.set_bounds(Rect {
                size: dpi::Size::Logical(LogicalSize {
                    width: bounds.size.width.into(),
                    height: bounds.size.height.into(),
                }),
                position: dpi::Position::Logical(LogicalPosition::new(
                    bounds.origin.x.into(),
                    bounds.origin.y.into(),
                )),
            });
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        // A click GPUI dispatches never reached the native view — either it
        // landed outside the surface or on an overlay drawn above it. Either
        // way the page is not the click target, so hand keyboard focus back
        // to the window: overlay dismissal, the composer, and keybindings all
        // depend on the GPUI view being first responder again.
        let webview = self.view.clone();
        window.on_mouse_event(move |_: &MouseDownEvent, phase, _, _| {
            if phase == DispatchPhase::Bubble {
                let _ = webview.focus_parent();
            }
        });
    }
}
