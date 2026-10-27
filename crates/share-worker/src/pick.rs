//! Which window is the game's: the rule every system's capture shares, on plain numbers so that
//! it is tested everywhere. Windows lists its windows through `windows-capture` and keeps its own
//! loop in [`crate::capture`]; macOS lists them through ScreenCaptureKit ([`crate::mac::sck`]) and
//! asks this module which one to take.

use crate::capture::WindowTarget;

/// How large a window must be to count as the game's (the launcher's rule for the game window).
pub const MIN_WIDTH: f64 = 200.0;
pub const MIN_HEIGHT: f64 = 150.0;

/// A window a system listed.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    /// The system's number for the window.
    pub id: u32,
    /// The process it belongs to.
    pub pid: u32,
    pub width: f64,
    pub height: f64,
    /// Zero for an ordinary window; menus, docks and overlays sit on other layers.
    pub layer: i64,
    pub title: String,
}

impl Seen {
    fn big_enough(&self) -> bool {
        self.width >= MIN_WIDTH && self.height >= MIN_HEIGHT
    }

    fn area(&self) -> f64 {
        self.width * self.height
    }
}

/// The index of the window to capture, in this order:
///
/// 1. the window the launcher named, if it is listed, large enough, and (when processes are
///    given) belongs to one of them;
/// 2. the largest ordinary window of the game's processes that is large enough;
/// 3. only when no process is known: an ordinary, large window whose title contains the title.
///
/// A window of another process is never taken when processes are known.
pub fn pick(seen: &[Seen], target: &WindowTarget) -> Option<usize> {
    let owned = |w: &Seen| target.pids.is_empty() || target.pids.contains(&w.pid);
    if let Some(id) = target
        .window
        .filter(|&id| id > 0 && id <= i64::from(u32::MAX))
    {
        let named = seen
            .iter()
            .position(|w| i64::from(w.id) == id && w.big_enough() && owned(w));
        if named.is_some() {
            return named;
        }
    }
    let ordinary = |w: &Seen| w.layer == 0 && w.big_enough();
    if !target.pids.is_empty() {
        return seen
            .iter()
            .enumerate()
            .filter(|(_, w)| target.pids.contains(&w.pid) && ordinary(w))
            .max_by(|a, b| a.1.area().total_cmp(&b.1.area()))
            .map(|(index, _)| index);
    }
    if !target.title_contains.is_empty() {
        return seen
            .iter()
            .position(|w| ordinary(w) && w.title.contains(&target.title_contains));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u32, pid: u32, width: f64, height: f64, layer: i64, title: &str) -> Seen {
        Seen {
            id,
            pid,
            width,
            height,
            layer,
            title: title.into(),
        }
    }

    fn target(window: Option<i64>, pids: &[u32], title: &str) -> WindowTarget {
        WindowTarget {
            window,
            pids: pids.to_vec(),
            title_contains: title.into(),
        }
    }

    #[test]
    fn the_largest_ordinary_window_of_the_game_wins() {
        let seen = [
            window(1, 10, 400.0, 300.0, 0, "Launcher"),
            window(2, 20, 1920.0, 1080.0, 0, "Other program"),
            window(3, 10, 1280.0, 720.0, 0, "Game"),
            window(4, 10, 3000.0, 40.0, 25, "Overlay"),
            window(5, 10, 100.0, 100.0, 0, "Tiny"),
        ];
        assert_eq!(pick(&seen, &target(None, &[10], "")), Some(2));
    }

    #[test]
    fn a_window_of_another_process_is_never_taken_when_processes_are_known() {
        let seen = [window(2, 20, 1920.0, 1080.0, 0, "Game")];
        assert_eq!(pick(&seen, &target(None, &[10], "Game")), None);
    }

    #[test]
    fn the_named_window_counts_only_while_it_belongs_to_the_game() {
        let seen = [
            window(7, 99, 800.0, 600.0, 0, "Reused number"),
            window(8, 10, 640.0, 480.0, 0, "Game"),
        ];
        // The number 7 now belongs to another process: believe the processes.
        assert_eq!(pick(&seen, &target(Some(7), &[10], "")), Some(1));
        assert_eq!(pick(&seen, &target(Some(8), &[10], "")), Some(1));
        assert_eq!(pick(&seen, &target(Some(7), &[], "")), Some(0));
    }

    #[test]
    fn the_title_is_used_only_without_processes() {
        let seen = [
            window(1, 10, 800.0, 600.0, 0, "My Game - Level 3"),
            window(2, 11, 800.0, 600.0, 0, "Browser"),
        ];
        assert_eq!(pick(&seen, &target(None, &[], "My Game")), Some(0));
        assert_eq!(pick(&seen, &target(None, &[], "Missing")), None);
        assert_eq!(pick(&seen, &target(None, &[], "")), None);
    }

    #[test]
    fn nothing_is_picked_from_an_empty_list() {
        assert_eq!(pick(&[], &target(Some(1), &[1], "x")), None);
    }
}
