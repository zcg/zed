//! Windows Tab Coordinator
//!
//! Coordinates windows that are visually grouped as tabs on Windows platform.
//! Since Windows has no native window tabbing support like macOS, this module
//! implements "virtual tabbing" by managing window visibility and positioning.

use std::cell::RefCell;
use std::collections::HashMap;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowPlacement, GetWindowRect, IsIconic, IsWindow, IsZoomed, SW_HIDE, SW_MAXIMIZE, SW_SHOW,
    SW_SHOWNA, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos, ShowWindow,
    WINDOWPLACEMENT,
};

/// Coordinates windows that are visually grouped as tabs.
///
/// This coordinator manages multiple HWNDs that belong to the same "tab group"
/// identified by a tabbing identifier string. When switching tabs:
/// 1. The current active window is hidden
/// 2. The target window is moved to the same position
/// 3. The target window is shown and activated
pub struct WindowsTabCoordinator {
    /// Map of tabbing_identifier -> list of HWNDs in that tab group
    tab_groups: RefCell<HashMap<String, Vec<HWND>>>,
    /// The currently visible/active window per tab group
    active_tabs: RefCell<HashMap<String, HWND>>,
}

impl Default for WindowsTabCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsTabCoordinator {
    /// Create a new tab coordinator.
    pub fn new() -> Self {
        Self {
            tab_groups: RefCell::new(HashMap::new()),
            active_tabs: RefCell::new(HashMap::new()),
        }
    }

    /// Register a window with a tab group.
    pub fn register_window(&self, identifier: &str, hwnd: HWND) {
        if !Self::is_alive(hwnd) {
            return;
        }

        let mut groups = self.tab_groups.borrow_mut();
        let windows = groups.entry(identifier.to_string()).or_default();

        // Avoid duplicate registration
        if !windows.contains(&hwnd) {
            windows.push(hwnd);
        }

        // If this is the first window in the group, mark it as active
        let mut active = self.active_tabs.borrow_mut();
        if !active.contains_key(identifier) {
            active.insert(identifier.to_string(), hwnd);
        }
    }

    /// Unregister a window from its tab group.
    pub fn unregister_window(&self, identifier: &str, hwnd: HWND) {
        let mut groups = self.tab_groups.borrow_mut();
        if let Some(windows) = groups.get_mut(identifier) {
            windows.retain(|h| *h != hwnd);

            // If the group is now empty, remove it
            if windows.is_empty() {
                groups.remove(identifier);
                self.active_tabs.borrow_mut().remove(identifier);
            } else {
                // If we removed the active tab, select a new one
                let mut active = self.active_tabs.borrow_mut();
                if active.get(identifier) == Some(&hwnd) {
                    if let Some(first) = windows.first() {
                        active.insert(identifier.to_string(), *first);
                    }
                }
            }
        }
    }

    /// Called when a window is being destroyed while part of a tab group.
    ///
    /// If the destroyed window was the active/visible tab, we attempt to show another
    /// window from the same group so the app doesn't "disappear" with only hidden tabs left.
    pub fn handle_window_destroyed(&self, identifier: &str, hwnd: HWND) {
        let closing_rect = Self::get_window_rect(hwnd);
        let closing_was_zoomed = Self::is_alive(hwnd) && unsafe { IsZoomed(hwnd).as_bool() };

        let next_to_show = {
            let mut groups = self.tab_groups.borrow_mut();
            let mut active = self.active_tabs.borrow_mut();

            let Some(windows) = groups.get_mut(identifier) else {
                active.remove(identifier);
                return;
            };

            windows.retain(|h| *h != hwnd);

            if windows.is_empty() {
                groups.remove(identifier);
                active.remove(identifier);
                return;
            }

            if active.get(identifier) == Some(&hwnd) {
                let next = windows[0];
                active.insert(identifier.to_string(), next);
                Some(next)
            } else {
                None
            }
        };

        let Some(next_hwnd) = next_to_show else {
            return;
        };
        if !Self::is_alive(next_hwnd) {
            self.prune_invalid_handles(identifier);
            return;
        }

        unsafe {
            if let Some(rect) = closing_rect {
                let _ = SetWindowPos(
                    next_hwnd,
                    None,
                    rect.left,
                    rect.top,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }

            let show_cmd = if closing_was_zoomed {
                SW_MAXIMIZE
            } else {
                SW_SHOW
            };
            let _ = ShowWindow(next_hwnd, show_cmd);
        }
    }

    /// Returns a snapshot of every HWND known to the coordinator.
    pub fn all_windows(&self) -> Vec<HWND> {
        let groups = self.tab_groups.borrow();
        let mut all = Vec::new();
        for windows in groups.values() {
            all.extend(windows.iter().copied());
        }
        all.sort_by_key(|h| h.0 as usize);
        all.dedup();
        all
    }

    /// Switch to a specific tab in a group (core method).
    ///
    /// This will:
    /// 1. Get the position of the currently active window
    /// 2. Hide the currently active window
    /// 3. Move the target window to the same position
    /// 4. Show and activate the target window (preserving maximized state)
    pub fn activate_tab(&self, identifier: &str, target_hwnd: HWND) {
        self.prune_invalid_handles(identifier);

        if !Self::is_alive(target_hwnd) {
            self.unregister_window(identifier, target_hwnd);
            return;
        }

        let previous = {
            let groups = self.tab_groups.borrow();
            let Some(windows) = groups.get(identifier) else {
                return;
            };

            // Verify target is in this group
            if !windows.contains(&target_hwnd) {
                return;
            }

            self.active_tabs.borrow().get(identifier).copied()
        };

        // If target is already active, nothing to do
        if previous == Some(target_hwnd) {
            return;
        }

        // Commit the new active tab up-front so re-entrant queries during Win32 dispatch
        // see the intended state.
        self.active_tabs
            .borrow_mut()
            .insert(identifier.to_string(), target_hwnd);

        let position = previous.and_then(Self::get_window_rect);
        let previous_is_zoomed = previous
            .is_some_and(|prev| Self::is_alive(prev) && unsafe { IsZoomed(prev).as_bool() });

        // NOTE: Avoid holding any RefCell borrows across Win32 calls, since those calls can
        // synchronously deliver messages and re-enter GPUI/window code.

        // IMPORTANT: Show the new window BEFORE hiding the old one to avoid flicker.
        // If we hide first, there's a brief moment where no window is visible, causing a flash.
        unsafe {
            if let Some(rect) = position {
                let _ = SetWindowPos(
                    target_hwnd,
                    None,
                    rect.left,
                    rect.top,
                    0, // width ignored with SWP_NOSIZE
                    0, // height ignored with SWP_NOSIZE
                    // Reposition + show, but don't steal activation here. The caller's `activate()`
                    // path handles foregrounding (and its Windows quirks) consistently.
                    SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
        }

        unsafe {
            // Preserve maximized state from the previous window
            let show_cmd = if previous_is_zoomed {
                SW_MAXIMIZE
            } else {
                SW_SHOWNA
            };
            let _ = ShowWindow(target_hwnd, show_cmd);
        }

        // Hide the previous window AFTER showing the new one to prevent flicker
        if let Some(prev) = previous {
            if prev != target_hwnd && Self::is_alive(prev) {
                unsafe {
                    let _ = ShowWindow(prev, SW_HIDE);
                }
            }
        }
    }

    /// Get the window rectangle.
    fn get_window_rect(hwnd: HWND) -> Option<RECT> {
        if !Self::is_alive(hwnd) {
            return None;
        }

        // When a window is minimized, `GetWindowRect` returns the icon rectangle, which is not
        // what we want for tab positioning. `GetWindowPlacement` gives us the restored bounds.
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let mut placement = WINDOWPLACEMENT {
                    length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                    ..Default::default()
                };
                GetWindowPlacement(hwnd, &mut placement).ok()?;
                return Some(placement.rcNormalPosition);
            }
        }

        let mut rect = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut rect).ok()? };
        Some(rect)
    }

    /// Get all windows in a tab group.
    pub fn get_group_windows(&self, identifier: &str) -> Vec<HWND> {
        self.tab_groups
            .borrow()
            .get(identifier)
            .cloned()
            .unwrap_or_default()
    }

    /// Merge a single window into an existing tab group without changing which window is active.
    ///
    /// This is used for "drag a tab into another window" on Windows. Since Windows doesn't have
    /// native window tabbing, we simulate tabbing by hiding the merged window and positioning it
    /// under the active window in the target group.
    pub fn absorb_window_into_group(&self, identifier: &str, target_hwnd: HWND, hwnd: HWND) {
        self.prune_invalid_handles(identifier);

        if hwnd == target_hwnd {
            return;
        }

        if !Self::is_alive(target_hwnd) || !Self::is_alive(hwnd) {
            return;
        }

        // Be resilient to minor coordinator/controller ordering differences: ensure both HWNDs
        // are registered in this group before manipulating them.
        self.register_window(identifier, target_hwnd);
        self.register_window(identifier, hwnd);

        let target_rect = Self::get_window_rect(target_hwnd);

        unsafe {
            if let Some(ref rect) = target_rect {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    rect.top,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }

    /// Merge all windows into a single tab group.
    ///
    /// This moves all windows from all groups into the target group,
    /// positions them at the target window's location, and hides all
    /// except the target window.
    pub fn merge_all(&self, target_identifier: &str, target_hwnd: HWND) {
        self.prune_all_invalid_handles();

        if !Self::is_alive(target_hwnd) {
            return;
        }

        // Snapshot the universe of windows before mutating state.
        let all_windows = self.all_windows();
        let target_rect = Self::get_window_rect(target_hwnd);

        {
            let mut groups = self.tab_groups.borrow_mut();
            let mut active = self.active_tabs.borrow_mut();

            groups.clear();
            groups.insert(target_identifier.to_string(), all_windows.clone());
            active.clear();
            active.insert(target_identifier.to_string(), target_hwnd);
        }

        // Hide/position everything except the target. No RefCell borrows held here.
        for hwnd in all_windows {
            if hwnd == target_hwnd || !Self::is_alive(hwnd) {
                continue;
            }

            unsafe {
                if let Some(ref rect) = target_rect {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        rect.left,
                        rect.top,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                }
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }

        // Make sure the target remains visible after the merge.
        unsafe {
            let _ = ShowWindow(target_hwnd, SW_SHOWNA);
        }
    }

    /// Move a window to a new independent tab group.
    ///
    /// This removes the window from its current group and creates
    /// a new group just for this window.
    pub fn move_to_new_group(&self, hwnd: HWND) -> Option<String> {
        self.prune_all_invalid_handles();

        if !Self::is_alive(hwnd) {
            return None;
        }

        // Determine which group currently contains this window, and which window (if any) should
        // become visible in that group after removal.
        let next_active_to_show = {
            let mut groups = self.tab_groups.borrow_mut();
            let mut active = self.active_tabs.borrow_mut();

            let mut next_active_to_show: Option<HWND> = None;
            let mut found_identifier: Option<String> = None;

            for (identifier, windows) in groups.iter_mut() {
                if let Some(pos) = windows.iter().position(|h| *h == hwnd) {
                    windows.remove(pos);
                    found_identifier = Some(identifier.clone());

                    if active.get(identifier) == Some(&hwnd) {
                        if let Some(first) = windows.first().copied() {
                            active.insert(identifier.clone(), first);
                            next_active_to_show = Some(first);
                        } else {
                            active.remove(identifier);
                        }
                    }
                    break;
                }
            }

            if let Some(ref id) = found_identifier
                && groups.get(id).is_some_and(|w| w.is_empty())
            {
                groups.remove(id);
                active.remove(id);
            }

            next_active_to_show
        };

        // Show the new active window (if any) after releasing RefCell borrows.
        if let Some(hwnd) = next_active_to_show
            && Self::is_alive(hwnd)
        {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNA);
            }
        }

        // Create a new unique identifier for this window.
        let new_identifier = format!("zed-{}", hwnd.0 as usize);
        {
            let mut groups = self.tab_groups.borrow_mut();
            let mut active = self.active_tabs.borrow_mut();
            groups.insert(new_identifier.clone(), vec![hwnd]);
            active.insert(new_identifier.clone(), hwnd);
        }

        // Show the window in its new position (offset slightly).
        unsafe {
            if let Some(mut rect) = Self::get_window_rect(hwnd) {
                rect.left += 30;
                rect.top += 30;
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    rect.top,
                    0,
                    0,
                    SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
            let _ = ShowWindow(hwnd, SW_SHOWNA);
        }

        // We intentionally don't attempt to foreground here; callers should decide when to
        // activate, so activation flows through a single path.
        Some(new_identifier)
    }

    fn is_alive(hwnd: HWND) -> bool {
        !hwnd.is_invalid() && unsafe { IsWindow(Some(hwnd)).as_bool() }
    }

    fn prune_invalid_handles(&self, identifier: &str) {
        // Avoid holding RefCell borrows across Win32 calls (`IsWindow`), which may re-enter
        // window/message dispatch paths.
        let current_windows = {
            let groups = self.tab_groups.borrow();
            groups.get(identifier).cloned()
        };

        let Some(current_windows) = current_windows else {
            self.active_tabs.borrow_mut().remove(identifier);
            return;
        };

        let alive_windows: Vec<HWND> = current_windows
            .into_iter()
            .filter(|h| Self::is_alive(*h))
            .collect();

        if alive_windows.is_empty() {
            self.tab_groups.borrow_mut().remove(identifier);
            self.active_tabs.borrow_mut().remove(identifier);
            return;
        }

        let mut groups = self.tab_groups.borrow_mut();
        let mut active = self.active_tabs.borrow_mut();

        let Some(windows) = groups.get_mut(identifier) else {
            active.remove(identifier);
            return;
        };

        *windows = alive_windows;

        if let Some(active_hwnd) = active.get(identifier).copied()
            && !windows.contains(&active_hwnd)
        {
            active.insert(identifier.to_string(), windows[0]);
        } else if !active.contains_key(identifier) {
            active.insert(identifier.to_string(), windows[0]);
        }
    }

    fn prune_all_invalid_handles(&self) {
        let identifiers: Vec<String> = self.tab_groups.borrow().keys().cloned().collect();
        for identifier in identifiers {
            self.prune_invalid_handles(&identifier);
        }
    }
}
