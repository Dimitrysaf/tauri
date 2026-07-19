// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  path::PathBuf,
  sync::{Arc, Mutex},
};

use cef::*;
use tauri_runtime::{
  UserEvent,
  dpi::PhysicalPosition,
  webview::InitializationScript,
  window::{DragDropEvent, WindowId},
};
use url::Url;

use crate::runtime::{Message, RuntimeContext};

const DRAG_DROP_BRIDGE_PATH: &str = "/__tauri_cef_drag_drop__";

/// Bridge path used by [`WINDOW_DRAG_INIT_SCRIPT`] to report the window origin
/// the frontend wants while a `data-tauri-drag-region` drag is in progress.
#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
pub(crate) const WINDOW_DRAG_BRIDGE_PATH: &str = "/__tauri_cef_window_drag__";

/// Moves the window from the renderer while a drag region is held.
///
/// `start_dragging` hands off to the window manager via `_NET_WM_MOVERESIZE`,
/// which requires the WM to grab the pointer. It cannot: pressing a mouse
/// button inside the browser creates an implicit X11 grab owned by CEF's own X
/// connection, and winit's `drag_initiate` can only ungrab its own connection,
/// so the WM's move loop never receives the pointer and the window never moves.
///
/// CEF holding the grab does mean the renderer keeps receiving motion events,
/// so the drag is driven from here instead: remember where inside the window
/// the pointer grabbed, then keep asking for `pointer - grab` as the new origin.
/// Absolute positions are used rather than deltas so the window cannot drift.
#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
const WINDOW_DRAG_INIT_SCRIPT: &str = r#"
(() => {
  if (window.__TAURI_CEF_WINDOW_DRAG__) {
    return;
  }

  Object.defineProperty(window, "__TAURI_CEF_WINDOW_DRAG__", {
    value: true,
    configurable: false,
  });

  const PATH = "/__tauri_cef_window_drag__";

  // Mirrors the drag region rules in tauri's own drag.js.
  const CLICKABLE_TAGS = new Set([
    "A", "BUTTON", "INPUT", "SELECT", "TEXTAREA", "LABEL", "SUMMARY"
  ]);
  const INTERACTIVE_ROLES = new Set([
    "button", "link", "menuitem", "tab", "checkbox", "radio", "switch", "option"
  ]);

  const isClickableElement = (el) =>
    CLICKABLE_TAGS.has(el.tagName)
    || (el.hasAttribute("contenteditable")
      && el.getAttribute("contenteditable") !== "false")
    || (el.hasAttribute("tabindex") && el.getAttribute("tabindex") !== "-1")
    || INTERACTIVE_ROLES.has(el.getAttribute("role"));

  function isDragRegion(composedPath) {
    for (const el of composedPath) {
      if (!(el instanceof HTMLElement)) continue;
      const attr = el.getAttribute("data-tauri-drag-region");
      if (isClickableElement(el) && attr === null) return false;
      if (attr === null) continue;
      if (attr === "false") return false;
      if (attr === "deep") return true;
      if (attr === "" || attr === "true") return el === composedPath[0];
    }
    return false;
  }

  // Edge bitmask, matching ResizeGesture on the Rust side.
  const WEST = 1, EAST = 2, NORTH = 4, SOUTH = 8;
  // How close to an edge (CSS px) counts as a resize handle. An undecorated
  // winit window has no frame, so there is nothing else providing one.
  const BORDER = 6;

  const CURSORS = {
    [WEST]: "ew-resize", [EAST]: "ew-resize",
    [NORTH]: "ns-resize", [SOUTH]: "ns-resize",
    [NORTH | WEST]: "nwse-resize", [SOUTH | EAST]: "nwse-resize",
    [NORTH | EAST]: "nesw-resize", [SOUTH | WEST]: "nesw-resize",
  };

  const edgeAt = (event) => {
    let edge = 0;
    if (event.clientX <= BORDER) edge |= WEST;
    else if (event.clientX >= window.innerWidth - BORDER) edge |= EAST;
    if (event.clientY <= BORDER) edge |= NORTH;
    else if (event.clientY >= window.innerHeight - BORDER) edge |= SOUTH;
    return edge;
  };

  let grabX = 0;
  let grabY = 0;
  let dragging = false;
  let resizing = 0;
  let cursorEdge = 0;
  let pending = null;
  let frame = 0;

  const post = (payload) => {
    const url = new URL(PATH, window.location.href);
    url.searchParams.set("payload", JSON.stringify(payload));
    fetch(url.href, {
      method: "GET",
      cache: "no-store",
      credentials: "omit",
    }).catch(() => {});
  };

  const flush = () => {
    frame = 0;
    if (!pending) return;
    const payload = pending;
    pending = null;
    post(payload);
  };

  const stop = () => {
    dragging = false;
    resizing = 0;
    pending = null;
  };

  const setCursor = (edge) => {
    if (edge === cursorEdge) return;
    cursorEdge = edge;
    document.documentElement.style.cursor = edge ? CURSORS[edge] || "" : "";
  };

  // Capture phase: tauri's drag.js calls stopImmediatePropagation() when it
  // matches a drag region, which would otherwise hide the mousedown from us.
  window.addEventListener("mousedown", (event) => {
    if (event.button !== 0 || event.detail !== 1) return;
    const ratio = window.devicePixelRatio || 1;

    // A resize edge wins over a drag region: the titlebar reaches the window
    // border, so its top and side edges must still resize.
    const edge = edgeAt(event);
    if (edge) {
      resizing = edge;
      // Keep the press away from the page, which would otherwise see a click on
      // whatever sits under the border.
      event.preventDefault();
      event.stopImmediatePropagation();
      post({
        mode: "resize",
        edge,
        start: true,
        x: Math.round(event.screenX * ratio),
        y: Math.round(event.screenY * ratio),
      });
      return;
    }

    if (!isDragRegion(event.composedPath())) return;
    grabX = event.clientX * ratio;
    grabY = event.clientY * ratio;
    dragging = true;
  }, { capture: true });

  window.addEventListener("mousemove", (event) => {
    const ratio = window.devicePixelRatio || 1;

    if (!dragging && !resizing) {
      setCursor(edgeAt(event));
      return;
    }

    // The button can be released outside the webview, where no mouseup arrives.
    if (!(event.buttons & 1)) {
      stop();
      setCursor(0);
      return;
    }

    pending = resizing
      ? {
          mode: "resize",
          edge: resizing,
          start: false,
          x: Math.round(event.screenX * ratio),
          y: Math.round(event.screenY * ratio),
        }
      : {
          x: Math.round(event.screenX * ratio - grabX),
          y: Math.round(event.screenY * ratio - grabY),
        };
    if (!frame) frame = requestAnimationFrame(flush);
  }, { capture: true });

  window.addEventListener("mouseup", stop, { capture: true });
  window.addEventListener("blur", () => { stop(); setCursor(0); }, { capture: true });
  document.addEventListener("mouseleave", () => {
    if (!dragging && !resizing) setCursor(0);
  }, { capture: true });
})();
"#;

#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
pub(crate) fn window_drag_initialization_script() -> InitializationScript {
  InitializationScript {
    script: WINDOW_DRAG_INIT_SCRIPT.to_string(),
    for_main_frame_only: true,
  }
}

/// Window origin requested by [`WINDOW_DRAG_INIT_SCRIPT`], in physical pixels.
#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
#[derive(Clone, serde::Deserialize)]
pub(crate) struct WindowDragScriptEvent {
  pub(crate) x: i32,
  pub(crate) y: i32,
  /// Absent for a move; `"resize"` when an edge is being dragged.
  #[serde(default)]
  pub(crate) mode: Option<String>,
  #[serde(default)]
  pub(crate) edge: u8,
  #[serde(default)]
  pub(crate) start: bool,
}

const DRAG_DROP_INIT_SCRIPT: &str = r#"
(() => {
  if (window.__TAURI_CEF_DRAG_DROP__) {
    return;
  }

  Object.defineProperty(window, "__TAURI_CEF_DRAG_DROP__", {
    value: true,
    configurable: false,
  });

  const PATH = "/__tauri_cef_drag_drop__";
  let entered = false;

  const position = (event) => ({
    x: event.clientX * window.devicePixelRatio,
    y: event.clientY * window.devicePixelRatio,
  });

  const send = (type, event) => {
    const pos = position(event);
    const url = new URL(PATH, window.location.href);
    url.searchParams.set("payload", JSON.stringify({ type, x: pos.x, y: pos.y }));
    fetch(url.href, {
      method: "GET",
      cache: "no-store",
      credentials: "omit",
    }).catch(() => {});
  };

  const listen = (eventName, handler) => {
    window.addEventListener(eventName, handler, { capture: true });
  };

  listen("dragenter", (event) => {
    if (!entered) {
      entered = true;
      send("enter", event);
    }
  });

  listen("dragover", (event) => {
    if (!entered) {
      entered = true;
      send("enter", event);
    }
    send("over", event);
  });

  listen("drop", (event) => {
    if (!entered) {
      send("enter", event);
    }
    entered = false;
    send("drop", event);
  });

  listen("dragleave", (event) => {
    const x = event.clientX;
    const y = event.clientY;
    if (entered && (x <= 0 || y <= 0 || x >= window.innerWidth || y >= window.innerHeight)) {
      entered = false;
      send("leave", event);
    }
  });
})();
"#;

pub(crate) fn drag_drop_initialization_script() -> InitializationScript {
  InitializationScript {
    script: DRAG_DROP_INIT_SCRIPT.to_string(),
    for_main_frame_only: false,
  }
}

#[derive(Default)]
pub(crate) struct DragDropState {
  pub(crate) paths: Option<Vec<PathBuf>>,
  pub(crate) native_entered: bool,
  pub(crate) entered: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragDropEventTarget {
  Window,
  Webview,
}

#[derive(Clone, serde::Deserialize)]
pub(crate) struct DragDropScriptEvent {
  #[serde(rename = "type")]
  pub(crate) kind: String,
  pub(crate) x: f64,
  pub(crate) y: f64,
}

fn collect_drag_data_paths(drag_data: &mut DragData) -> Vec<PathBuf> {
  let mut paths = CefStringList::new();
  if drag_data.file_paths(Some(&mut paths)) != 0 {
    let paths = paths
      .into_iter()
      .filter(|path| !path.is_empty())
      .map(PathBuf::from)
      .collect::<Vec<_>>();

    if !paths.is_empty() {
      return paths;
    }
  }

  let file_name = CefStringUtf16::from(&drag_data.file_name()).to_string();
  if file_name.is_empty() {
    Vec::new()
  } else {
    vec![PathBuf::from(file_name)]
  }
}

wrap_drag_handler! {
  pub struct TauriCefDragHandler {
    drag_drop_state: Arc<Mutex<DragDropState>>,
  }

  impl DragHandler {
    fn on_drag_enter(
      &self,
      _browser: Option<&mut Browser>,
      drag_data: Option<&mut DragData>,
      _mask: DragOperationsMask,
    ) -> ::std::os::raw::c_int {
      let mut state = self.drag_drop_state.lock().unwrap();
      state.entered = false;
      state.paths = drag_data
        .map(collect_drag_data_paths)
        .filter(|paths| !paths.is_empty());
      state.native_entered = state.paths.is_some();

      // Let Chromium continue with the drag operation so the injected script can
      // report over/drop/leave with accurate viewport positions.
      0
    }
  }
}

pub(crate) fn event_from_script_event(
  drag_drop_state: &Arc<Mutex<DragDropState>>,
  script_event: DragDropScriptEvent,
) -> Option<DragDropEvent> {
  let position = PhysicalPosition::new(script_event.x, script_event.y);
  let mut state = drag_drop_state.lock().unwrap();
  if !state.native_entered {
    return None;
  }

  match script_event.kind.as_str() {
    "enter" => {
      if state.entered {
        return None;
      }

      let paths = state.paths.clone()?;
      state.entered = true;
      Some(DragDropEvent::Enter { paths, position })
    }
    "over" => state.entered.then_some(DragDropEvent::Over { position }),
    "drop" => {
      let paths = state.entered.then(|| state.paths.take()).flatten();
      state.entered = false;
      state.native_entered = false;
      paths.map(|paths| DragDropEvent::Drop { paths, position })
    }
    "leave" => {
      state.native_entered = false;
      state.paths = None;

      if state.entered {
        state.entered = false;
        Some(DragDropEvent::Leave)
      } else {
        None
      }
    }
    _ => None,
  }
}

wrap_resource_request_handler! {
  pub(crate) struct WebDragDropResourceRequestHandler<T: UserEvent> {
    context: RuntimeContext<T>,
    window_id: WindowId,
    webview_id: u32,
    drag_drop_event_target: DragDropEventTarget,
    drag_drop_handler_enabled: bool,
    drag_drop_state: Arc<Mutex<DragDropState>>,
  }

  impl ResourceRequestHandler {
    fn on_before_resource_load(
      &self,
      _browser: Option<&mut Browser>,
      _frame: Option<&mut Frame>,
      request: Option<&mut Request>,
      _callback: Option<&mut Callback>,
    ) -> ReturnValue {
      // Window dragging is independent of the drag-and-drop handler.
      #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      if let Some(request) = &request {
        let url = CefString::from(&request.url()).to_string();
        if let Ok(url) = Url::parse(&url)
          && url.path() == WINDOW_DRAG_BRIDGE_PATH
        {
          if let Some(payload) = url
            .query_pairs()
            .find_map(|(key, value)| (key == "payload").then(|| value.into_owned()))
            && let Ok(event) = serde_json::from_str::<WindowDragScriptEvent>(&payload)
          {
            let message = if event.mode.as_deref() == Some("resize") {
              crate::window::WindowMessage::ResizeToPointer {
                edge: event.edge,
                x: event.x,
                y: event.y,
                start: event.start,
              }
            } else {
              crate::window::WindowMessage::SetPosition(
                tauri_runtime::dpi::PhysicalPosition::new(event.x, event.y).into(),
              )
            };
            let _ = self.context.send_message(Message::Window {
              window_id: self.window_id,
              message,
            });
          }

          return sys::cef_return_value_t::RV_CANCEL.into();
        }
      }

      if self.drag_drop_handler_enabled
        && let Some(request) = request
      {
        let url = CefString::from(&request.url()).to_string();
        if let Ok(url) = Url::parse(&url)
          && url.path() == DRAG_DROP_BRIDGE_PATH
        {
          if let Some(payload) = url
            .query_pairs()
            .find_map(|(key, value)| (key == "payload").then(|| value.into_owned()))
            && let Ok(event) = serde_json::from_str::<DragDropScriptEvent>(&payload)
          {
            let _ = self.context.send_message(Message::DragDropScriptEvent {
              window_id: self.window_id,
              webview_id: self.webview_id,
              target: self.drag_drop_event_target,
              drag_drop_state: self.drag_drop_state.clone(),
              event,
            });
          }

          return sys::cef_return_value_t::RV_CANCEL.into();
        }
      }

      sys::cef_return_value_t::RV_CONTINUE.into()
    }
  }
}
