//! The dashboard's state machine: selection, filter, sort, modes, and what
//! each key does. No I/O, so it is unit-tested directly.

use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::actions::{self, Activation, RunsOn};
use crate::config::Host;
use crate::model::{AppState, DiffOutcome, HostId, HostState, Status};

/// How long a flash message stays in the footer.
pub const FLASH_FOR: Duration = Duration::from_secs(3);
/// Spinner frame interval while something is running.
pub const TICK: Duration = Duration::from_millis(80);
/// Age labels (`12m ago`) are redrawn at least this often.
const AGE_REFRESH: Duration = Duration::from_secs(30);
/// Longest filter accepted, in characters (it has to fit the footer).
const MAX_FILTER: usize = 64;
/// Rows a page key moves before a frame has told us the real height.
const DEFAULT_PAGE: u16 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    /// Most urgent first ([`crate::model::Status`] order), then name.
    Status,
}

/// What follows a confirmed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    /// Activation: probe the host again.
    Reprobe,
    /// Build for a diff: diff again.
    Rediff,
}

/// A command waiting for `y`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub id: HostId,
    /// `Activate bigmem?` / `Build the new generation on bigmem?`
    pub title: String,
    /// Exactly what will run.
    pub command: String,
    /// `here` or `on <host> (<destination>)`.
    pub runs_on: String,
    pub warning: Option<String>,
    pub after: After,
    /// Return to the diff view (instead of the table) when dismissed.
    pub from_diff: bool,
    /// The first line shown of a command too long for its box (which
    /// shows all of it, scrolled).
    pub scroll: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing a filter (`/`).
    Filter,
    Help,
    Confirm(Confirm),
    /// Full-screen closure diff of a host.
    Diff {
        id: HostId,
        scroll: u16,
    },
}

/// What the event loop should do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    None,
    Quit,
    /// Re-run the whole check.
    Refresh,
    /// Compute the closure diff of a host.
    Diff(HostId),
    /// Run a confirmed command interactively for a host, then `after`.
    Run {
        id: HostId,
        command: String,
        after: After,
    },
    /// Put text on the clipboard (OSC 52).
    Copy(String),
}

/// A scroll limit that no frame has measured yet.
pub(super) const UNKNOWN: u16 = u16::MAX;

/// What the last frame looked like, recorded by the view through `&App`
/// (atomics keep `App` `Sync`): page sizes and scroll limits for the keys,
/// and the hosts table's scroll offset, which only moves when the selection
/// leaves the visible rows.
#[derive(Debug)]
pub(super) struct Geometry {
    /// Host rows the table shows (0 before the first frame).
    pub(super) table_rows: AtomicU16,
    /// Display index of the table's first visible row.
    pub(super) table_offset: AtomicUsize,
    /// Change rows the diff view shows (0 before the first frame).
    pub(super) diff_rows: AtomicU16,
    /// Largest useful diff scroll ([`UNKNOWN`] before the first frame).
    pub(super) diff_max: AtomicU16,
    /// Largest useful details scroll ([`UNKNOWN`] before the first frame).
    pub(super) detail_max: AtomicU16,
    /// Lines of its command a confirmation shows at once (0 before the
    /// first frame).
    pub(super) confirm_rows: AtomicU16,
    /// Largest useful confirmation scroll, 0 when it all fits
    /// ([`UNKNOWN`] before the first frame).
    pub(super) confirm_max: AtomicU16,
}

impl Default for Geometry {
    fn default() -> Self {
        Geometry {
            table_rows: AtomicU16::new(0),
            table_offset: AtomicUsize::new(0),
            diff_rows: AtomicU16::new(0),
            diff_max: AtomicU16::new(UNKNOWN),
            detail_max: AtomicU16::new(UNKNOWN),
            confirm_rows: AtomicU16::new(0),
            confirm_max: AtomicU16::new(UNKNOWN),
        }
    }
}

pub struct App {
    pub state: AppState,
    pub mode: Mode,
    pub filter: String,
    pub sort: SortKey,
    /// The host the cursor is on (by id, so it survives re-sorting).
    selected: Option<HostId>,
    /// Scroll offset of the details pane (reset when the selection changes).
    pub detail_scroll: u16,
    flash: Option<(String, Instant)>,
    /// Spinner frame, advanced by [`App::tick`].
    pub spinner: usize,
    pub color: bool,
    geometry: Geometry,
    /// The diff view's scroll while a confirmation opened from it is shown.
    diff_return_scroll: u16,
}

impl App {
    pub fn new(state: AppState, color: bool) -> App {
        App {
            state,
            mode: Mode::Normal,
            filter: String::new(),
            sort: SortKey::Name,
            selected: None,
            detail_scroll: 0,
            flash: None,
            spinner: 0,
            color,
            geometry: Geometry::default(),
            diff_return_scroll: 0,
        }
    }

    /// Swap in `state`, whose hosts may not be the current ones (the flake's
    /// changed): the selection and an open diff view follow their host by
    /// name; a confirmation is dropped unless its host is still exactly
    /// the same (it would run what was confirmed for another definition).
    /// Whether a confirmation was dropped.
    pub fn replace_state(&mut self, state: AppState) -> bool {
        let old = std::mem::replace(&mut self.state, state);
        let new = &self.state;
        let renumber = |id: HostId| {
            let name = &old.hosts.get(id)?.host.name;
            new.hosts.iter().position(|h| h.host.name == *name)
        };
        self.selected = self.selected.and_then(renumber);
        let mut dropped = false;
        self.mode = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Diff { id, scroll } => match renumber(id) {
                Some(id) => Mode::Diff { id, scroll },
                None => Mode::Normal,
            },
            Mode::Confirm(confirm) => {
                let same = |id: HostId| old.hosts[confirm.id].host == new.hosts[id].host;
                match renumber(confirm.id).filter(|&id| same(id)) {
                    Some(id) => Mode::Confirm(Confirm { id, ..confirm }),
                    None => {
                        dropped = true;
                        Mode::Normal
                    }
                }
            }
            mode => mode,
        };
        dropped
    }

    /// Hosts shown, in display order: those matching the filter (a glob if
    /// it contains `*`, `?` or `[`, else a case-insensitive substring of the
    /// name), sorted by [`SortKey`] (ties by name).
    pub fn visible(&self) -> Vec<HostId> {
        let filter = self.filter.as_str();
        let glob = filter.contains(['*', '?', '[']);
        let needle = if glob {
            String::new()
        } else {
            filter.to_lowercase()
        };
        let matches = |name: &str| {
            filter.is_empty()
                || if glob {
                    crate::util::glob_match(filter, name)
                } else {
                    contains_ignore_case(name, &needle)
                }
        };
        let hosts = &self.state.hosts;
        let name = |id: HostId| hosts[id].host.name.as_str();
        match self.sort {
            SortKey::Name => {
                let mut ids: Vec<HostId> =
                    (0..hosts.len()).filter(|&id| matches(name(id))).collect();
                ids.sort_unstable_by(|&a, &b| name(a).cmp(name(b)).then(a.cmp(&b)));
                ids
            }
            SortKey::Status => {
                let mut keyed: Vec<(Status, HostId)> = (0..hosts.len())
                    .filter(|&id| matches(name(id)))
                    .map(|id| (hosts[id].status(), id))
                    .collect();
                keyed.sort_unstable_by(|&(sa, a), &(sb, b)| {
                    sa.cmp(&sb)
                        .then_with(|| name(a).cmp(name(b)))
                        .then(a.cmp(&b))
                });
                keyed.into_iter().map(|(_, id)| id).collect()
            }
        }
    }

    /// The remembered selection if still visible, else the first visible host.
    pub fn selected(&self) -> Option<HostId> {
        self.selected_in(&self.visible())
    }

    /// [`selected`](Self::selected) given [`visible`](Self::visible)'s result.
    pub(super) fn selected_in(&self, visible: &[HostId]) -> Option<HostId> {
        match self.selected {
            Some(id) if visible.contains(&id) => Some(id),
            _ => visible.first().copied(),
        }
    }

    pub fn select(&mut self, id: HostId) {
        if self.selected != Some(id) {
            self.detail_scroll = 0;
        }
        self.selected = Some(id);
    }

    /// The activation of `id` ([`crate::actions::activation`]) given its
    /// desired generation and resolved flake, or why there is none
    /// ("desired generation not known yet", the action's own reasons).
    pub fn activation(&self, id: HostId) -> Result<Activation, String> {
        let h = self
            .state
            .hosts
            .get(id)
            .ok_or_else(|| "no such host".to_string())?;
        // While a new evaluation runs, the old desired generation may not
        // belong to the flake revision that is now resolved.
        if h.eval.is_running() {
            return Err("desired generation not known yet (evaluating)".into());
        }
        let desired = match h.eval.get() {
            Some(Ok(desired)) => desired,
            Some(Err(_)) => return Err("its configuration doesn't evaluate".into()),
            None => return Err("desired generation not known yet".into()),
        };
        let flake = self
            .state
            .flake_info(&h.host.flake)
            .ok_or_else(|| "its flake isn't resolved".to_string())?;
        actions::activation(&h.host, flake, &desired.path)
    }

    /// Handle a key press. Normal mode: `j`/`k`/arrows, `g`/`G`/Home/End,
    /// PgUp/PgDn move; `d`/Enter open the diff view (and start a diff unless
    /// one is current or running); `a` asks to activate; `y` copies
    /// ([`crate::actions::copy_text`]); `r` refreshes (flash if a check
    /// runs); `/` filter; `s` sort; `?` help; `J`/`K` scroll details;
    /// `q`/Ctrl-C quit; Esc clears the filter. Diff view: `j`/`k`/PgUp/PgDn
    /// scroll, `b` asks to build when the desired generation is missing,
    /// `q`/Esc back. Confirm: only `y` runs (Enter just flashes the keys),
    /// `n`/Esc/`q` cancel, the diff view's scrolling keys scroll a command
    /// too long for its box. Actions on a busy host and unavailable actions
    /// set a flash explaining why.
    ///
    /// Also: Ctrl-C quits from every mode; in the filter prompt, Enter
    /// keeps the filter, Esc drops it, Ctrl-U/Ctrl-W erase and ↑/↓ move;
    /// any key closes the help; the diff view also has Space/`g`/`G`/Home/
    /// End, `a` (activate), `y` (copy) and `r` (diff again). Other keys
    /// with Ctrl or Alt are ignored, so a chord never triggers an action.
    pub fn on_key(&mut self, key: KeyEvent, now: Instant) -> Command {
        if key.kind == KeyEventKind::Release {
            return Command::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Command::Quit;
        }
        match self.mode {
            Mode::Normal => self.normal_key(key, now),
            Mode::Filter => {
                self.filter_key(key);
                Command::None
            }
            Mode::Help => {
                self.mode = Mode::Normal;
                Command::None
            }
            Mode::Confirm(_) => self.confirm_key(key, now),
            Mode::Diff { id, scroll } => self.diff_key(id, scroll, key, now),
        }
    }

    /// Advance the spinner.
    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
    }

    pub fn flash(&mut self, message: impl Into<String>, now: Instant) {
        self.flash = Some((message.into(), now));
    }

    /// The flash message, if younger than [`FLASH_FOR`].
    pub fn current_flash(&self, now: Instant) -> Option<&str> {
        match &self.flash {
            Some((message, at)) if now.duration_since(*at) < FLASH_FOR => Some(message),
            _ => None,
        }
    }

    /// When the screen next needs redrawing without input: the next spinner
    /// frame while anything is busy, the flash expiry, `refresh_at`, and at
    /// least every 30 s (age labels). `None` never happens in practice but
    /// keeps the type honest for tests.
    pub fn next_wakeup(&self, now: Instant, refresh_at: Option<Instant>) -> Option<Instant> {
        let mut next = now + AGE_REFRESH;
        if self.state.is_busy() {
            next = next.min(now + TICK);
        }
        if let Some((_, at)) = &self.flash {
            let expires = *at + FLASH_FOR;
            if expires > now {
                next = next.min(expires);
            }
        }
        if let Some(refresh_at) = refresh_at {
            next = next.min(refresh_at);
        }
        Some(next)
    }

    /// What the view records about each frame.
    pub(super) fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    /// The diff view's scroll offset, also while a confirmation opened from
    /// the diff view is shown on top of it.
    pub(super) fn diff_scroll(&self) -> u16 {
        match &self.mode {
            Mode::Diff { scroll, .. } => *scroll,
            Mode::Confirm(c) if c.from_diff => self.diff_return_scroll,
            _ => 0,
        }
    }

    fn normal_key(&mut self, key: KeyEvent, now: Instant) -> Command {
        if has_chord(key) {
            return Command::None;
        }
        let page = i32::from(page(self.geometry.table_rows.load(Ordering::Relaxed)));
        match key.code {
            KeyCode::Char('q') => return Command::Quit,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(Move::By(1)),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(Move::By(-1)),
            KeyCode::PageDown => self.move_selection(Move::By(page)),
            KeyCode::PageUp => self.move_selection(Move::By(-page)),
            KeyCode::Char('g') | KeyCode::Home => self.move_selection(Move::First),
            KeyCode::Char('G') | KeyCode::End => self.move_selection(Move::Last),
            KeyCode::Char('d') | KeyCode::Enter => return self.open_diff(now),
            KeyCode::Char('a') => self.ask_activation(None, now),
            KeyCode::Char('y') => return self.copy(None, now),
            KeyCode::Char('r') => {
                if self.state.run.is_running() {
                    self.flash("a check is already running", now);
                } else {
                    return Command::Refresh;
                }
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('s') => {
                let (sort, message) = match self.sort {
                    SortKey::Name => (SortKey::Status, "sorted by status"),
                    SortKey::Status => (SortKey::Name, "sorted by name"),
                };
                self.sort = sort;
                self.flash(message, now);
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('J') => self.scroll_details(1),
            KeyCode::Char('K') => self.scroll_details(-1),
            KeyCode::Esc if !self.filter.is_empty() => {
                let before = self.selected();
                self.filter.clear();
                if self.selected() != before {
                    self.detail_scroll = 0;
                }
            }
            _ => {}
        }
        Command::None
    }

    fn filter_key(&mut self, key: KeyEvent) {
        let before = self.selected();
        self.edit_filter(key);
        if self.selected() != before {
            self.detail_scroll = 0;
        }
    }

    fn edit_filter(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Down => self.move_selection(Move::By(1)),
            KeyCode::Up => self.move_selection(Move::By(-1)),
            KeyCode::Char('u') if ctrl => self.filter.clear(),
            KeyCode::Char('w') if ctrl => {
                let kept = self
                    .filter
                    .trim_end()
                    .trim_end_matches(|c: char| !c.is_whitespace());
                self.filter.truncate(kept.len());
            }
            KeyCode::Char(c)
                if !has_chord(key)
                    && !c.is_control()
                    && self.filter.chars().count() < MAX_FILTER =>
            {
                self.filter.push(c);
            }
            _ => {}
        }
    }

    fn confirm_key(&mut self, key: KeyEvent, now: Instant) -> Command {
        if has_chord(key) {
            return Command::None;
        }
        match key.code {
            // Only a deliberate `y` runs it: Enter is what opened the diff
            // a screen earlier.
            KeyCode::Enter => {
                self.flash("y runs it, n or esc cancels", now);
                Command::None
            }
            KeyCode::Char('y') => {
                let Mode::Confirm(confirm) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return Command::None;
                };
                // A build is followed by a new diff, so its view is where the
                // result shows up; an activation makes the diff obsolete.
                if confirm.from_diff && confirm.after == After::Rediff {
                    self.mode = Mode::Diff {
                        id: confirm.id,
                        scroll: 0,
                    };
                }
                Command::Run {
                    id: confirm.id,
                    command: confirm.command,
                    after: confirm.after,
                }
            }
            KeyCode::Char('n' | 'q') | KeyCode::Esc => {
                if let Mode::Confirm(confirm) = &self.mode {
                    self.mode = if confirm.from_diff {
                        Mode::Diff {
                            id: confirm.id,
                            scroll: self.diff_return_scroll,
                        }
                    } else {
                        Mode::Normal
                    };
                }
                Command::None
            }
            _ => {
                self.scroll_confirm(key);
                Command::None
            }
        }
    }

    /// The diff view's scrolling keys, for a command too long for its
    /// confirmation box.
    fn scroll_confirm(&mut self, key: KeyEvent) {
        let page = i32::from(page(self.geometry.confirm_rows.load(Ordering::Relaxed)));
        let max = self.geometry.confirm_max.load(Ordering::Relaxed);
        let Mode::Confirm(confirm) = &mut self.mode else {
            return;
        };
        let scroll = confirm.scroll;
        confirm.scroll = match key.code {
            KeyCode::Char('j') | KeyCode::Down => step(scroll, 1, max),
            KeyCode::Char('k') | KeyCode::Up => step(scroll, -1, max),
            KeyCode::PageDown | KeyCode::Char(' ') => step(scroll, page, max),
            KeyCode::PageUp => step(scroll, -page, max),
            KeyCode::Char('g') | KeyCode::Home => 0,
            KeyCode::Char('G') | KeyCode::End => max,
            _ => scroll,
        };
    }

    fn diff_key(&mut self, id: HostId, scroll: u16, key: KeyEvent, now: Instant) -> Command {
        if has_chord(key) {
            return Command::None;
        }
        let page = i32::from(page(self.geometry.diff_rows.load(Ordering::Relaxed)));
        let max = self.diff_max(id);
        let to = |delta: i32| step(scroll, delta, max);
        let scroll = match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                return Command::None;
            }
            KeyCode::Char('j') | KeyCode::Down => to(1),
            KeyCode::Char('k') | KeyCode::Up => to(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => to(page),
            KeyCode::PageUp => to(-page),
            KeyCode::Char('g') | KeyCode::Home => 0,
            KeyCode::Char('G') | KeyCode::End => max,
            KeyCode::Char('b') => {
                self.ask_build(id, scroll, now);
                return Command::None;
            }
            KeyCode::Char('a') => {
                self.ask_activation(Some((id, scroll)), now);
                return Command::None;
            }
            KeyCode::Char('y') => return self.copy(Some(id), now),
            KeyCode::Char('r') => return self.rediff(id, now),
            _ => return Command::None,
        };
        self.mode = Mode::Diff { id, scroll };
        Command::None
    }

    /// The largest useful diff scroll: what the last frame measured, else
    /// one change per row.
    fn diff_max(&self, id: HostId) -> u16 {
        let measured = self.geometry.diff_max.load(Ordering::Relaxed);
        if measured != UNKNOWN {
            return measured;
        }
        match self.state.hosts.get(id).and_then(|h| h.diff.get()) {
            Some(Ok(DiffOutcome::Diff { diff })) => {
                u16::try_from(diff.changes.len().saturating_sub(1)).unwrap_or(u16::MAX)
            }
            _ => 0,
        }
    }

    fn move_selection(&mut self, to: Move) {
        let visible = self.visible();
        let Some(last) = visible.len().checked_sub(1) else {
            return;
        };
        let current = self.selected_in(&visible);
        let at = current
            .and_then(|id| visible.iter().position(|&v| v == id))
            .unwrap_or(0);
        let next = match to {
            Move::By(delta) => at.saturating_add_signed(delta as isize).min(last),
            Move::First => 0,
            Move::Last => last,
        };
        let id = visible[next];
        if current == Some(id) {
            // Pinning the shown selection isn't a change (keep the scroll).
            self.selected = Some(id);
        } else {
            self.select(id);
        }
    }

    fn scroll_details(&mut self, delta: i32) {
        let max = self.geometry.detail_max.load(Ordering::Relaxed);
        self.detail_scroll = step(self.detail_scroll, delta, max);
    }

    fn open_diff(&mut self, now: Instant) -> Command {
        let Some(id) = self.selected() else {
            self.flash("no host selected", now);
            return Command::None;
        };
        let h = &self.state.hosts[id];
        if let Some(problem) = diff_problem(h) {
            self.flash(problem, now);
            return Command::None;
        }
        let current = diff_is_current(h);
        self.mode = Mode::Diff { id, scroll: 0 };
        if current {
            Command::None
        } else {
            Command::Diff(id)
        }
    }

    fn rediff(&mut self, id: HostId, now: Instant) -> Command {
        let Some(h) = self.state.hosts.get(id) else {
            return Command::None;
        };
        let problem = if h.diff.is_running() {
            Some(format!("a diff of {} is already running", h.host.name))
        } else {
            diff_problem(h)
        };
        match problem {
            Some(problem) => {
                self.flash(problem, now);
                Command::None
            }
            None => Command::Diff(id),
        }
    }

    /// Ask to activate the selected host, or `from_diff`'s host (whose diff
    /// view is at that scroll offset).
    fn ask_activation(&mut self, from_diff: Option<(HostId, u16)>, now: Instant) {
        let Some(id) = from_diff.map(|(id, _)| id).or_else(|| self.selected()) else {
            self.flash("no host selected", now);
            return;
        };
        let Some(h) = self.state.hosts.get(id) else {
            return;
        };
        if let Some(busy) = busy_reason(h) {
            self.flash(busy, now);
            return;
        }
        let confirm = match self.activation(id) {
            Ok(activation) => {
                let up_to_date = (h.status() == Status::UpToDate).then(|| {
                    format!(
                        "{} is already up to date: this changes nothing",
                        h.host.name
                    )
                });
                Confirm {
                    id,
                    title: format!("Activate {}?", h.host.name),
                    runs_on: runs_on_label(&h.host, activation.runs_on),
                    command: activation.command,
                    warning: join_warnings(activation.warning, up_to_date),
                    after: After::Reprobe,
                    from_diff: from_diff.is_some(),
                    scroll: 0,
                }
            }
            Err(why) => {
                let message = refusal("can't activate", "", &h.host.name, &why);
                self.flash(message, now);
                return;
            }
        };
        if let Some((_, scroll)) = from_diff {
            self.diff_return_scroll = scroll;
        }
        self.mode = Mode::Confirm(confirm);
    }

    fn ask_build(&mut self, id: HostId, scroll: u16, now: Instant) {
        let Some(h) = self.state.hosts.get(id) else {
            return;
        };
        let blocked = if !matches!(h.diff.get(), Some(Ok(DiffOutcome::DesiredMissing))) {
            Some("b builds the new generation when it is missing".to_string())
        } else {
            busy_reason(h)
        };
        if let Some(blocked) = blocked {
            self.flash(blocked, now);
            return;
        }
        let built = match self.state.flake_info(&h.host.flake) {
            Some(flake) => actions::build_command(&h.host, flake),
            None => Err("its flake isn't resolved".into()),
        };
        let command = match built {
            Ok(command) => command,
            Err(why) => {
                let message = refusal("can't build", "for", &h.host.name, &why);
                self.flash(message, now);
                return;
            }
        };
        let here = actions::runs_here(&h.host);
        let title = if here && !h.host.is_local() {
            format!("Build the new generation of {} here?", h.host.name)
        } else {
            format!("Build the new generation on {}?", h.host.name)
        };
        let runs_on = runs_on_label(&h.host, if here { RunsOn::Here } else { RunsOn::Remote });
        self.diff_return_scroll = scroll;
        self.mode = Mode::Confirm(Confirm {
            id,
            title,
            command,
            runs_on,
            warning: None,
            after: After::Rediff,
            from_diff: true,
            scroll: 0,
        });
    }

    /// Copy the activation of `id` (default: the selected host).
    fn copy(&mut self, id: Option<HostId>, now: Instant) -> Command {
        let Some(id) = id.or_else(|| self.selected()) else {
            self.flash("no host selected", now);
            return Command::None;
        };
        let Some(h) = self.state.hosts.get(id) else {
            return Command::None;
        };
        let copied = self
            .activation(id)
            .and_then(|activation| actions::copy_text(&h.host, &activation, &self.state.command));
        match copied {
            Ok(text) => {
                self.flash(format!("copied: {text}"), now);
                Command::Copy(text)
            }
            Err(why) => {
                let message = refusal("nothing to copy", "for", &h.host.name, &why);
                self.flash(message, now);
                Command::None
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Move {
    By(i32),
    First,
    Last,
}

/// Ctrl or Alt held: not one of our plain keys.
fn has_chord(key: KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn page(measured: u16) -> u16 {
    if measured == 0 {
        DEFAULT_PAGE
    } else {
        measured
    }
}

/// `scroll + delta`, kept within `0..=max`.
fn step(scroll: u16, delta: i32, max: u16) -> u16 {
    (i32::from(scroll) + delta).clamp(0, i32::from(max)) as u16
}

/// ASCII-case-insensitive `haystack.contains(needle)` without allocating;
/// `needle` is already lowercase. Non-ASCII names fall back to lowercasing.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if !haystack.is_ascii() || !needle.is_ascii() {
        return haystack.to_lowercase().contains(needle);
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// `<what> [<by>] <name>: <why>`, or `<what>: <why>` when the reason already
/// names the host (the actions' own reasons do).
fn refusal(what: &str, by: &str, name: &str, why: &str) -> String {
    let names_it = why
        .split_whitespace()
        .any(|word| word.trim_matches(|c: char| "'\"():,;".contains(c)) == name);
    match (names_it, by) {
        (true, _) => format!("{what}: {why}"),
        (false, "") => format!("{what} {name}: {why}"),
        (false, by) => format!("{what} {by} {name}: {why}"),
    }
}

/// Why `h` is busy, for refusing actions.
fn busy_reason(h: &HostState) -> Option<String> {
    h.busy().then(|| {
        format!(
            "{} is busy ({})",
            h.host.name,
            h.activity().unwrap_or("working")
        )
    })
}

/// Why `h` can't be diffed right now, if it can't: its probe or
/// evaluation is in flight (the inputs are about to change), or the
/// current or desired generation is unknown, or they are equal.
pub(super) fn diff_problem(h: &HostState) -> Option<String> {
    let name = &h.host.name;
    if h.probe.is_running() || h.eval.is_running() {
        return busy_reason(h);
    }
    let Some(current) = h.current() else {
        return Some(match h.probe.get() {
            Some(Ok(report)) => match &report.none_reason {
                Some(why) => format!("{name} has no generation to compare: {why}"),
                None => format!("{name} has no generation to compare"),
            },
            Some(Err(_)) => format!(
                "{name}'s current generation is unknown ({})",
                h.status().label()
            ),
            None => format!("{name} hasn't been probed yet"),
        });
    };
    let Some(desired) = h.desired() else {
        return Some(match h.eval.get() {
            Some(Err(_)) => format!("{name}'s configuration doesn't evaluate"),
            _ => format!("{name}'s desired generation isn't known yet"),
        });
    };
    (*current == desired.path).then(|| format!("{name} is up to date: nothing to diff"))
}

/// A diff for the host's current paths exists (successfully) or is running.
fn diff_is_current(h: &HostState) -> bool {
    let inputs_match = match (&h.diff_inputs, h.current(), h.desired()) {
        (Some((c, d)), Some(current), Some(desired)) => c == current && *d == desired.path,
        _ => false,
    };
    inputs_match && (h.diff.is_running() || matches!(h.diff.get(), Some(Ok(_))))
}

/// `here`, or `on <host> (<destination>)` (the destination only when it
/// says more than the name).
fn runs_on_label(host: &Host, runs_on: RunsOn) -> String {
    match (runs_on, host.ssh()) {
        (RunsOn::Here, _) => "here".into(),
        (RunsOn::Remote, Some(ssh)) if ssh.destination != host.name => {
            format!("on {} ({})", host.name, ssh.destination)
        }
        (RunsOn::Remote, _) => format!("on {}", host.name),
    }
}

fn join_warnings(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    }
}

/// Hosts and states shared by the app and view tests.
#[cfg(test)]
pub(super) mod testing {
    use std::time::{Duration, Instant};

    use crate::cache::DeployedRev;
    use crate::closure::{Change, ClosureDiff};
    use crate::config::{ActivateOn, Host, Kind, Sources, SshTarget, Target};
    use crate::model::{
        AppState, Desired, DiffOutcome, ErrorInfo, Event, HostId, ProbeError, ProbeFailure,
    };
    use crate::nix::FlakeInfo;
    use crate::probe::ProbeReport;
    use crate::storepath::StorePath;

    pub const FLAKE: &str = "github:jjant/nix-config";
    pub const REV: &str = "42ea902131b93c04d098d3bcd02f29305b4bad7c";
    /// `lastModified` of the fixture flake (tests/fixtures/metadata-github.json).
    pub const LAST_MODIFIED: i64 = 1_792_240_011;
    /// "Now" in every test: 2h after the fixture flake's last commit.
    pub const NOW_UNIX: i64 = LAST_MODIFIED + 2 * 3600;
    /// The roots of tests/fixtures/closure-hm-{1,9}.
    pub const HM_1: &str = "/nix/store/494amg2bizplp1s8485gr6b2k6pl4mxh-home-manager-generation";
    pub const HM_9: &str = "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-home-manager-generation";

    pub fn ssh_host(name: &str, kind: Kind, destination: &str) -> Host {
        Host {
            name: name.into(),
            kind,
            flake: FLAKE.into(),
            attr: kind
                .default_attr(name)
                .unwrap_or_else(|| format!("packages.{name}")),
            target: Target::Ssh(SshTarget {
                destination: destination.into(),
                user: None,
                host: destination.into(),
                port: None,
                options: vec![],
                forced_command: None,
            }),
            profile: None,
            activate: kind.default_activate().map(str::to_string),
            activate_on: ActivateOn::Target,
            optional: false,
            note: None,
            sources: Sources::default(),
        }
    }

    pub fn local_host(name: &str, kind: Kind) -> Host {
        Host {
            target: Target::Local { assumed: false },
            ..ssh_host(name, kind, name)
        }
    }

    pub fn flake_info() -> FlakeInfo {
        FlakeInfo {
            flake: FLAKE.into(),
            locked: format!(
                "github:jjant/nix-config/{REV}?narHash=sha256-rTwky4CPWade4oeZ81h5PcDt8%2Be8kXznGgaiDqYFhFk%3D"
            ),
            kind: Some("github".into()),
            locked_url: None,
            rev: Some(REV.into()),
            dirty_rev: None,
            last_modified: Some(LAST_MODIFIED),
            rev_count: None,
            fingerprint: Some(
                "773b1c0a8f4cbe4ff5c09fd6c963dc4df2680450367f592cf85c58dd280bfc74".into(),
            ),
            nar_hash: Some("sha256-rTwky4CPWade4oeZ81h5PcDt8+e8kXznGgaiDqYFhFk=".into()),
            description: Some("jjant's nix config".into()),
            stale: None,
            lock_outdated: false,
        }
    }

    /// A store path whose hash is `c` repeated (must be Nix base32).
    pub fn path(c: char, name: &str) -> StorePath {
        StorePath::parse(&format!("/nix/store/{}-{name}", c.to_string().repeat(32)))
            .expect("valid test store path")
    }

    pub fn store(path: &str) -> StorePath {
        StorePath::parse(path).expect("valid test store path")
    }

    pub fn report(current: StorePath, generation: u64, activated: i64) -> ProbeReport {
        ProbeReport {
            platform: Some("x86_64-linux".into()),
            profile: Some(current.clone()),
            current: Some(current),
            generation: Some(generation),
            activated: Some(activated),
            ..ProbeReport::default()
        }
    }

    pub fn desired(path: StorePath, cached: bool) -> Desired {
        Desired {
            path,
            rev: Some(REV.into()),
            cached,
        }
    }

    /// Feeds events at fixed offsets from one start instant.
    pub struct Script {
        pub state: AppState,
        pub t0: Instant,
    }

    impl Script {
        pub fn new(hosts: Vec<Host>) -> Script {
            Script {
                state: AppState::new(hosts),
                t0: Instant::now(),
            }
        }

        pub fn at(&self, ms: u64) -> Instant {
            self.t0 + Duration::from_millis(ms)
        }

        pub fn apply(&mut self, ms: u64, event: Event) -> &mut Self {
            let at = self.at(ms);
            self.state.apply(event, at);
            self
        }

        pub fn flake(&mut self, ms: u64) -> &mut Self {
            self.apply(0, Event::FlakeStarted { url: FLAKE.into() });
            self.apply(
                ms,
                Event::FlakeFinished {
                    url: FLAKE.into(),
                    result: Ok(flake_info()),
                },
            )
        }

        pub fn probed(&mut self, id: HostId, ms: u64, report: ProbeReport) -> &mut Self {
            self.probed_with(id, ms, report, None)
        }

        pub fn probed_with(
            &mut self,
            id: HostId,
            ms: u64,
            report: ProbeReport,
            deployed: Option<DeployedRev>,
        ) -> &mut Self {
            self.apply(0, Event::ProbeStarted(id));
            self.apply(
                ms,
                Event::ProbeFinished {
                    id,
                    result: Ok(report),
                    deployed,
                },
            )
        }

        pub fn probe_failed(
            &mut self,
            id: HostId,
            ms: u64,
            kind: ProbeFailure,
            error: ErrorInfo,
        ) -> &mut Self {
            self.apply(0, Event::ProbeStarted(id));
            self.apply(
                ms,
                Event::ProbeFinished {
                    id,
                    result: Err(ProbeError { kind, error }),
                    deployed: None,
                },
            )
        }

        pub fn evaluated(&mut self, id: HostId, ms: u64, desired: Desired) -> &mut Self {
            if !desired.cached {
                self.apply(0, Event::EvalStarted(id));
            }
            self.apply(
                ms,
                Event::EvalFinished {
                    id,
                    result: Ok(desired),
                },
            )
        }

        pub fn eval_failed(&mut self, id: HostId, ms: u64, error: ErrorInfo) -> &mut Self {
            self.apply(0, Event::EvalStarted(id));
            self.apply(
                ms,
                Event::EvalFinished {
                    id,
                    result: Err(error),
                },
            )
        }

        pub fn diffed(
            &mut self,
            id: HostId,
            ms: u64,
            result: Result<DiffOutcome, ErrorInfo>,
        ) -> &mut Self {
            let h = &self.state.hosts[id];
            let current = h.current().cloned().expect("probed before diffing");
            let desired = h.desired().expect("evaluated before diffing").path.clone();
            self.apply(
                0,
                Event::DiffStarted {
                    id,
                    current: current.clone(),
                    desired: desired.clone(),
                },
            );
            self.apply(
                ms,
                Event::DiffFinished {
                    id,
                    current,
                    desired,
                    result,
                },
            )
        }
    }

    /// The real `nix store diff-closures` of tests/fixtures/closure-hm-1 →
    /// closure-hm-9 (diff-closures-1-9.txt), with sizes and path counts from
    /// the fixtures' `.tsv` files.
    pub fn fixture_diff() -> ClosureDiff {
        let change =
            |name: &str, removed: &[&str], added: &[&str], before: Option<u64>, after: u64| {
                Change {
                    name: name.into(),
                    removed: removed.iter().map(|s| s.to_string()).collect(),
                    added: added.iter().map(|s| s.to_string()).collect(),
                    in_before: before.is_some(),
                    in_after: true,
                    size_before: before.unwrap_or(0),
                    size_after: after,
                }
            };
        ClosureDiff {
            changes: vec![
                change("atuin", &[], &[""], Some(74_175_600), 74_175_776),
                change("atuin-server.service", &[], &[""], None, 920),
                change("hm_tmuxwindownaming", &[], &[""], None, 1_848),
                change("home-manager", &[], &[], Some(4_289_336), 4_300_392),
                change("zenoh", &[], &["1.10.0", "1.10.0-fish"], None, 348_055_896),
                change(
                    "zenoh-bridge-remote-api",
                    &[],
                    &["1.10.1", "1.10.1-fish"],
                    None,
                    33_140_288,
                ),
            ],
            paths_before: 695,
            paths_after: 702,
            size_before: 7_474_800_208,
            size_after: 7_856_012_864,
        }
    }

    /// The nix error of tests/fixtures/eval-ifd-read-only.stderr.
    pub fn ifd_error() -> ErrorInfo {
        let stderr = include_str!("../../tests/fixtures/eval-ifd-read-only.stderr");
        ErrorInfo::new(
            "path '/nix/store/wdfh0jh598v4qshmcs296ri45p0win4b-gen.drv' is not valid",
            stderr,
        )
    }

    /// One host per status and drift, as after a complete check:
    ///
    /// | id | host          | status                         |
    /// | -- | ------------- | ------------------------------ |
    /// | 0  | al2-aarch64   | outdated, behind (cache), note |
    /// | 1  | al2-x86_64    | not activated                  |
    /// | 2  | al2023-x86_64 | evaluating (pending)           |
    /// | 3  | bigmem        | outdated (local), unknown gen  |
    /// | 4  | builder       | probe failed                   |
    /// | 5  | desk          | eval failed (IFD fixture)      |
    /// | 6  | mac-m1        | offline (optional), note       |
    /// | 7  | pi            | unreachable                    |
    /// | 8  | web1          | up to date, reboot, special.   |
    /// | 9  | web2          | outdated, staged               |
    /// | 10 | web3          | outdated, same rev             |
    /// | 11 | web4          | up to date (cached)            |
    pub fn mixed() -> Script {
        let mut mac = ssh_host("mac-m1", Kind::Darwin, "ssh://devuser1@localhost:2022");
        mac.optional = true;
        mac.note = Some("reachable only from a desk you ssh into from the Mac".into());
        if let crate::config::Target::Ssh(ssh) = &mut mac.target {
            ssh.forced_command = Some("current-system".into());
        }
        let mut aarch64 = ssh_host(
            "al2-aarch64",
            Kind::HomeManager,
            "devhost-devuser1-2c-2b7c5f39.dc-west2.example.com",
        );
        aarch64.note = Some("graviton desk".into());
        let hosts = vec![
            aarch64,
            ssh_host(
                "al2-x86_64",
                Kind::HomeManager,
                "devhost-devuser1-1a-8d41e07a.dc-west1.example.com",
            ),
            ssh_host("al2023-x86_64", Kind::HomeManager, "al2023-x86_64"),
            local_host("bigmem", Kind::HomeManager),
            ssh_host("builder", Kind::Nixos, "root@builder"),
            ssh_host("desk", Kind::Nixos, "desk"),
            mac,
            ssh_host("pi", Kind::Nixos, "pi"),
            ssh_host("web1", Kind::Nixos, "web1"),
            ssh_host("web2", Kind::Darwin, "web2"),
            ssh_host("web3", Kind::Nixos, "web3"),
            ssh_host("web4", Kind::Nixos, "web4"),
        ];
        let mut s = Script::new(hosts);
        let hour = 3600;
        let day = 24 * hour;
        s.apply(0, Event::RunStarted).flake(300);

        // 0: behind a cached revision, 4 months old.
        s.probed_with(
            0,
            25,
            report(
                path('a', "home-manager-generation"),
                93,
                NOW_UNIX - 125 * day,
            ),
            Some(DeployedRev {
                flake: FLAKE.into(),
                rev: Some("1a2b3c4d5e6f".into()),
                last_modified: Some(NOW_UNIX - 126 * day),
            }),
        )
        .evaluated(
            0,
            3400,
            desired(path('b', "home-manager-generation"), false),
        );

        // 1: never activated.
        s.probed(
            1,
            31,
            crate::probe::ProbeReport {
                none_reason: Some("no home-manager profile".into()),
                platform: Some("x86_64-linux".into()),
                ..Default::default()
            },
        )
        .evaluated(
            1,
            3100,
            desired(path('c', "home-manager-generation"), false),
        );

        // 2: still evaluating (probe done).
        s.probed(
            2,
            40,
            report(path('d', "home-manager-generation"), 12, NOW_UNIX - 3 * day),
        );
        s.apply(1000, Event::EvalStarted(2));

        // 3: this machine, an unknown generation.
        s.probed(3, 12, report(store(HM_1), 9, NOW_UNIX - 12 * 60))
            .evaluated(3, 3300, desired(store(HM_9), false));

        // 4: probe failed.
        s.probe_failed(
            4,
            900,
            ProbeFailure::Failed,
            ErrorInfo::new(
                "probe output ended early (connection dropped?)",
                "ssh -T -o BatchMode=yes … root@builder sh -s\nexit 1, output ended after `platform=x86_64-linux`",
            ),
        )
        .evaluated(4, 2900, desired(path('f', "nixos-system-builder-25.11"), false));

        // 5: the configuration doesn't evaluate.
        s.probed(
            5,
            30,
            report(path('g', "nixos-system-desk-25.11"), 41, NOW_UNIX - 9 * day),
        )
        .eval_failed(5, 2500, ifd_error());

        // 6: optional and unreachable → offline.
        s.probe_failed(
            6,
            10_000,
            ProbeFailure::Unreachable,
            ErrorInfo::msg("ssh: connect to host localhost port 2022: Connection refused"),
        )
        .evaluated(6, 2200, desired(path('h', "darwin-system-25.11"), false));

        // 7: unreachable.
        s.probe_failed(
            7,
            10_000,
            ProbeFailure::Unreachable,
            ErrorInfo::new(
                "ssh: Could not resolve hostname pi: Name or service not known",
                "ssh -T -o BatchMode=yes … pi sh -s\nssh: Could not resolve hostname pi: Name or service not known\nhint: is `pi` in your ssh config or DNS?",
            ),
        )
        .evaluated(7, 2800, desired(path('i', "nixos-system-pi-25.11"), true));

        // 8: up to date, booted into an older kernel, in a specialisation.
        let web1 = path('j', "nixos-system-web1-25.11");
        s.probed(
            8,
            20,
            crate::probe::ProbeReport {
                current: Some(path('k', "specialisation-gaming")),
                toplevel: Some(web1.clone()),
                specialisation: Some("gaming".into()),
                reboot_required: true,
                booted: Some(path('l', "nixos-system-web1-25.11")),
                revision: Some(REV.into()),
                ..report(web1.clone(), 230, NOW_UNIX - 2 * hour)
            },
        )
        .evaluated(8, 4100, desired(web1, false));

        // 9: staged: the profile already points at the desired generation.
        let web2_new = path('m', "darwin-system-25.11");
        s.probed(
            9,
            22,
            crate::probe::ProbeReport {
                profile: Some(web2_new.clone()),
                ..report(path('n', "darwin-system-25.11"), 51, NOW_UNIX - day)
            },
        )
        .evaluated(9, 2000, desired(web2_new, false));

        // 10: same revision, different build.
        s.probed(
            10,
            19,
            crate::probe::ProbeReport {
                revision: Some(REV.into()),
                ..report(path('p', "nixos-system-web3-25.11"), 77, NOW_UNIX - 30 * 60)
            },
        )
        .evaluated(
            10,
            3000,
            desired(path('q', "nixos-system-web3-25.11"), false),
        );

        // 11: up to date, from the cache.
        let web4 = path('r', "nixos-system-web4-25.11");
        s.probed(11, 18, report(web4.clone(), 5, NOW_UNIX - 50 * day))
            .evaluated(11, 0, desired(web4, true));
        s
    }

    /// Every host still being checked: probes and evaluations in flight.
    pub fn pending() -> Script {
        let mut s = Script::new(vec![
            ssh_host("al2-aarch64", Kind::HomeManager, "al2-aarch64"),
            ssh_host("al2-x86_64", Kind::HomeManager, "al2-x86_64"),
            local_host("bigmem", Kind::HomeManager),
            ssh_host("mac-m1", Kind::Darwin, "mac-m1"),
            ssh_host("web1", Kind::Nixos, "web1"),
        ]);
        s.apply(0, Event::RunStarted)
            .apply(0, Event::FlakeStarted { url: FLAKE.into() });
        for id in 0..5 {
            s.apply(0, Event::ProbeStarted(id));
        }
        s.apply(
            12,
            Event::ProbeFinished {
                id: 2,
                result: Ok(report(store(HM_1), 9, NOW_UNIX - 12 * 60)),
                deployed: None,
            },
        );
        s
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::config::Kind;
    use crate::model::{ErrorInfo, Event, ProbeFailure};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        let modifiers = if c.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        KeyEvent::new(KeyCode::Char(c), modifiers)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn names(app: &App) -> Vec<&str> {
        app.visible()
            .into_iter()
            .map(|id| app.state.hosts[id].host.name.as_str())
            .collect()
    }

    fn selected_name(app: &App) -> Option<&str> {
        app.selected()
            .map(|id| app.state.hosts[id].host.name.as_str())
    }

    /// The mixed state after the check finished, so only host 2 is busy.
    fn mixed_app() -> (App, Instant) {
        let mut s = mixed();
        s.apply(10_000, Event::RunFinished);
        let now = s.at(12_000);
        (App::new(s.state, true), now)
    }

    fn press(app: &mut App, keys: &[KeyEvent], now: Instant) -> Vec<Command> {
        keys.iter().map(|&k| app.on_key(k, now)).collect()
    }

    fn select(app: &mut App, name: &str) {
        let id = app
            .state
            .hosts
            .iter()
            .position(|h| h.host.name == name)
            .expect("host exists");
        app.select(id);
    }

    /// `app`'s state with the hosts in `keep` only (in that order).
    fn only(app: &App, keep: &[&str]) -> AppState {
        let hosts = keep
            .iter()
            .map(|name| {
                let h = app.state.hosts.iter().find(|h| h.host.name == *name);
                h.expect("host exists").host.clone()
            })
            .collect();
        AppState::new(hosts)
    }

    #[test]
    fn a_new_host_list_keeps_the_selection_by_name() {
        let (mut app, _) = mixed_app();
        select(&mut app, "web2");
        let state = only(&app, &["web3", "web2", "bigmem"]);
        assert!(!app.replace_state(state));
        assert_eq!(
            app.selected()
                .map(|id| app.state.hosts[id].host.name.as_str()),
            Some("web2")
        );
        // An open diff view follows its host, or closes when it is gone.
        select(&mut app, "bigmem");
        app.mode = Mode::Diff { id: 2, scroll: 4 };
        app.replace_state(only(&app, &["bigmem", "web3"]));
        assert_eq!(app.mode, Mode::Diff { id: 0, scroll: 4 });
        app.replace_state(only(&app, &["web3"]));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.selected(), Some(0), "the first one, when it is gone");
    }

    #[test]
    fn a_confirmation_for_a_host_that_changed_is_dropped() {
        let (mut app, now) = mixed_app();
        select(&mut app, "al2-aarch64");
        app.on_key(ch('a'), now);
        let Mode::Confirm(confirm) = app.mode.clone() else {
            panic!("expected a confirmation, got {:?}", app.mode)
        };
        // Same host, another position: it stays, renumbered.
        let mut state = only(&app, &["web1", "al2-aarch64"]);
        assert!(!app.replace_state(state.clone()));
        assert_eq!(
            app.mode,
            Mode::Confirm(Confirm {
                id: 1,
                ..confirm.clone()
            })
        );
        // Redefined (another destination, say): what was confirmed isn't
        // what would run any more.
        state.hosts[1].host.note = Some("moved".into());
        if let crate::config::Target::Ssh(ssh) = &mut state.hosts[1].host.target {
            ssh.destination = "other.example.com".into();
        }
        assert!(app.replace_state(state));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn filters_by_case_insensitive_substring_or_glob() {
        let (mut app, _) = mixed_app();
        assert_eq!(names(&app).len(), 12);
        app.filter = "AL2".into();
        assert_eq!(names(&app), ["al2-aarch64", "al2-x86_64", "al2023-x86_64"]);
        app.filter = "x86".into();
        assert_eq!(names(&app), ["al2-x86_64", "al2023-x86_64"]);
        app.filter = "al2-*".into();
        assert_eq!(names(&app), ["al2-aarch64", "al2-x86_64"]);
        app.filter = "web?".into();
        assert_eq!(names(&app), ["web1", "web2", "web3", "web4"]);
        app.filter = "web[13]".into();
        assert_eq!(names(&app), ["web1", "web3"]);
        // A glob matches the whole name, a substring anywhere.
        app.filter = "eb*".into();
        assert!(names(&app).is_empty());
        app.filter = "eb".into();
        assert_eq!(names(&app).len(), 4);
        app.filter = "nothing-matches".into();
        assert!(names(&app).is_empty());
        assert_eq!(app.selected(), None);
    }

    #[test]
    fn substring_filters_handle_unicode_names() {
        let mut s = Script::new(vec![
            ssh_host("Mäc-Studio", Kind::Darwin, "studio"),
            ssh_host("pi", Kind::Nixos, "pi"),
        ]);
        s.apply(0, Event::RunFinished);
        let mut app = App::new(s.state, false);
        app.filter = "mäc".into();
        assert_eq!(names(&app), ["Mäc-Studio"]);
        assert!(contains_ignore_case("bigmem", ""));
        assert!(contains_ignore_case("BigMem", "gme"));
        assert!(!contains_ignore_case("big", "bigmem"));
    }

    #[test]
    fn sorts_by_name_or_status_with_ties_by_name() {
        let (mut app, now) = mixed_app();
        assert_eq!(
            names(&app),
            [
                "al2-aarch64",
                "al2-x86_64",
                "al2023-x86_64",
                "bigmem",
                "builder",
                "desk",
                "mac-m1",
                "pi",
                "web1",
                "web2",
                "web3",
                "web4"
            ]
        );
        assert_eq!(app.on_key(ch('s'), now), Command::None);
        assert_eq!(app.sort, SortKey::Status);
        assert_eq!(app.current_flash(now), Some("sorted by status"));
        assert_eq!(
            names(&app),
            [
                "desk",        // eval failed
                "builder",     // probe failed
                "pi",          // unreachable
                "al2-aarch64", // outdated…
                "bigmem",
                "web2",
                "web3",
                "al2-x86_64",    // not activated
                "al2023-x86_64", // pending
                "mac-m1",        // offline
                "web1",          // up to date
                "web4",
            ]
        );
        app.on_key(ch('s'), now);
        assert_eq!(app.sort, SortKey::Name);
        assert_eq!(app.current_flash(now), Some("sorted by name"));
    }

    #[test]
    fn selection_survives_sorting_and_falls_back_when_filtered_out() {
        let (mut app, now) = mixed_app();
        assert_eq!(selected_name(&app), Some("al2-aarch64"), "first by default");
        select(&mut app, "web1");
        app.on_key(ch('s'), now);
        assert_eq!(selected_name(&app), Some("web1"), "by id, not position");
        app.filter = "al2".into();
        assert_eq!(
            selected_name(&app),
            Some("al2-aarch64"),
            "hidden → first visible"
        );
        app.filter.clear();
        assert_eq!(selected_name(&app), Some("web1"), "remembered");
    }

    #[test]
    fn moves_with_vi_keys_arrows_and_pages() {
        let (mut app, now) = mixed_app();
        let at = |app: &App| selected_name(app).unwrap().to_string();
        press(&mut app, &[ch('j'), key(KeyCode::Down)], now);
        assert_eq!(at(&app), "al2023-x86_64");
        app.on_key(ch('k'), now);
        assert_eq!(at(&app), "al2-x86_64");
        app.on_key(key(KeyCode::Up), now);
        app.on_key(key(KeyCode::Up), now);
        assert_eq!(at(&app), "al2-aarch64", "clamped at the top");
        app.on_key(ch('G'), now);
        assert_eq!(at(&app), "web4");
        app.on_key(ch('j'), now);
        assert_eq!(at(&app), "web4", "clamped at the bottom");
        app.on_key(ch('g'), now);
        assert_eq!(at(&app), "al2-aarch64");
        app.on_key(key(KeyCode::End), now);
        assert_eq!(at(&app), "web4");
        app.on_key(key(KeyCode::Home), now);
        assert_eq!(at(&app), "al2-aarch64");
        // Pages are the table's height once a frame measured it.
        app.on_key(key(KeyCode::PageDown), now);
        assert_eq!(at(&app), "web3", "10 rows by default");
        app.geometry().table_rows.store(4, Ordering::Relaxed);
        app.on_key(key(KeyCode::PageUp), now);
        assert_eq!(at(&app), "mac-m1");
        app.on_key(key(KeyCode::PageUp), now);
        assert_eq!(at(&app), "al2023-x86_64");
        app.on_key(key(KeyCode::PageUp), now);
        assert_eq!(at(&app), "al2-aarch64");
        app.on_key(key(KeyCode::PageDown), now);
        assert_eq!(at(&app), "builder");
    }

    #[test]
    fn moving_resets_the_details_scroll() {
        let (mut app, now) = mixed_app();
        press(&mut app, &[ch('J'), ch('J'), ch('J')], now);
        assert_eq!(app.detail_scroll, 3);
        app.on_key(ch('K'), now);
        assert_eq!(app.detail_scroll, 2);
        app.on_key(ch('k'), now);
        assert_eq!(app.detail_scroll, 2, "already at the top: same host");
        app.on_key(ch('j'), now);
        assert_eq!(app.detail_scroll, 0);
        app.on_key(ch('K'), now);
        assert_eq!(app.detail_scroll, 0);
        // A measured limit bounds scrolling down.
        app.geometry().detail_max.store(1, Ordering::Relaxed);
        press(&mut app, &[ch('J'), ch('J'), ch('J')], now);
        assert_eq!(app.detail_scroll, 1);
    }

    #[test]
    fn filtering_away_the_selection_resets_the_details_scroll() {
        let (mut app, now) = mixed_app();
        select(&mut app, "web1");
        press(&mut app, &[ch('J'), ch('J')], now);
        app.on_key(ch('/'), now);
        app.on_key(ch('w'), now);
        assert_eq!(app.detail_scroll, 2, "web1 is still shown");
        app.on_key(ch('e'), now);
        app.on_key(ch('b'), now);
        app.on_key(ch('2'), now);
        assert_eq!(selected_name(&app), Some("web2"));
        assert_eq!(app.detail_scroll, 0);
        app.on_key(key(KeyCode::Enter), now);
        app.on_key(ch('J'), now);
        app.on_key(key(KeyCode::Esc), now);
        assert_eq!(
            selected_name(&app),
            Some("web1"),
            "the remembered host is back"
        );
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn moving_in_an_empty_list_is_harmless() {
        let mut app = App::new(AppState::default(), false);
        let now = Instant::now();
        for k in [ch('j'), ch('k'), ch('g'), ch('G'), key(KeyCode::PageDown)] {
            assert_eq!(app.on_key(k, now), Command::None);
        }
        assert_eq!(app.on_key(ch('d'), now), Command::None);
        assert_eq!(app.current_flash(now), Some("no host selected"));
        app.on_key(ch('a'), now);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.on_key(ch('y'), now), Command::None);
        assert_eq!(app.on_key(ch('r'), now), Command::Refresh);
    }

    #[test]
    fn filter_prompt_edits_and_applies() {
        let (mut app, now) = mixed_app();
        assert_eq!(app.on_key(ch('/'), now), Command::None);
        assert_eq!(app.mode, Mode::Filter);
        press(&mut app, &[ch('w'), ch('e'), ch('b'), ch('q')], now);
        assert_eq!(app.filter, "webq", "q is text while typing");
        assert_eq!(app.mode, Mode::Filter);
        app.on_key(key(KeyCode::Backspace), now);
        assert_eq!(app.filter, "web");
        assert_eq!(names(&app), ["web1", "web2", "web3", "web4"]);
        // Arrows move the selection among the matches while typing.
        app.on_key(key(KeyCode::Down), now);
        assert_eq!(selected_name(&app), Some("web2"));
        app.on_key(key(KeyCode::Up), now);
        assert_eq!(selected_name(&app), Some("web1"));
        assert_eq!(app.on_key(key(KeyCode::Enter), now), Command::None);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.filter, "web", "enter keeps the filter");
        // `/` again edits the same filter; Esc drops it.
        app.on_key(ch('/'), now);
        app.on_key(ch('1'), now);
        assert_eq!(names(&app), ["web1"]);
        app.on_key(key(KeyCode::Esc), now);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.filter, "");
        assert_eq!(names(&app).len(), 12);
    }

    #[test]
    fn filter_prompt_readline_keys_and_limits() {
        let (mut app, now) = mixed_app();
        app.on_key(ch('/'), now);
        for c in "al2 web".chars() {
            app.on_key(ch(c), now);
        }
        app.on_key(ctrl('w'), now);
        assert_eq!(app.filter, "al2 ");
        app.on_key(ctrl('w'), now);
        assert_eq!(app.filter, "");
        app.on_key(ctrl('w'), now);
        assert_eq!(app.filter, "");
        app.on_key(ch('x'), now);
        app.on_key(ctrl('u'), now);
        assert_eq!(app.filter, "");
        // Chords and control characters are not text.
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), now);
        app.on_key(
            KeyEvent::new(KeyCode::Char('\u{1b}'), KeyModifiers::NONE),
            now,
        );
        assert_eq!(app.filter, "");
        app.on_key(key(KeyCode::Backspace), now);
        assert_eq!(app.mode, Mode::Filter, "backspace on empty stays");
        for _ in 0..100 {
            app.on_key(ch('z'), now);
        }
        assert_eq!(app.filter.chars().count(), MAX_FILTER);
        assert_eq!(app.on_key(ctrl('c'), now), Command::Quit);
    }

    #[test]
    fn escape_clears_the_filter_in_normal_mode() {
        let (mut app, now) = mixed_app();
        app.filter = "web".into();
        assert_eq!(app.on_key(key(KeyCode::Esc), now), Command::None);
        assert_eq!(app.filter, "");
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.on_key(key(KeyCode::Esc), now),
            Command::None,
            "no filter: no-op"
        );
    }

    #[test]
    fn help_opens_and_any_key_closes_it() {
        let (mut app, now) = mixed_app();
        app.on_key(ch('?'), now);
        assert_eq!(app.mode, Mode::Help);
        assert_eq!(
            app.on_key(ch('q'), now),
            Command::None,
            "q closes the help, not the app"
        );
        assert_eq!(app.mode, Mode::Normal);
        app.on_key(ch('?'), now);
        app.on_key(ch('j'), now);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            selected_name(&app),
            Some("al2-aarch64"),
            "the key only closed the help"
        );
        app.on_key(ch('?'), now);
        assert_eq!(app.on_key(ctrl('c'), now), Command::Quit);
    }

    #[test]
    fn quit_keys() {
        let (mut app, now) = mixed_app();
        assert_eq!(app.on_key(ch('q'), now), Command::Quit);
        assert_eq!(app.on_key(ctrl('c'), now), Command::Quit);
        assert_eq!(
            app.on_key(
                KeyEvent::new(
                    KeyCode::Char('C'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ),
                now
            ),
            Command::Quit
        );
        for mode in [Mode::Filter, Mode::Help, Mode::Diff { id: 3, scroll: 0 }] {
            app.mode = mode;
            assert_eq!(app.on_key(ctrl('c'), now), Command::Quit);
        }
        select(&mut app, "bigmem");
        app.mode = Mode::Normal;
        app.on_key(ch('a'), now);
        assert!(matches!(app.mode, Mode::Confirm(_)));
        assert_eq!(app.on_key(ctrl('c'), now), Command::Quit);
    }

    #[test]
    fn key_releases_and_chords_do_nothing() {
        let (mut app, now) = mixed_app();
        let mut release = ch('q');
        release.kind = KeyEventKind::Release;
        assert_eq!(app.on_key(release, now), Command::None);
        select(&mut app, "bigmem");
        assert_eq!(app.on_key(ctrl('a'), now), Command::None);
        assert_eq!(app.mode, Mode::Normal, "ctrl-a doesn't activate");
        assert_eq!(app.on_key(ctrl('q'), now), Command::None);
        assert_eq!(app.on_key(ctrl('r'), now), Command::None);
        let mut repeat = ch('j');
        repeat.kind = KeyEventKind::Repeat;
        app.on_key(repeat, now);
        assert_eq!(selected_name(&app), Some("builder"), "held keys repeat");
    }

    #[test]
    fn refresh_is_refused_while_a_check_runs() {
        let (mut app, now) = mixed_app();
        assert_eq!(app.on_key(ch('r'), now), Command::Refresh);
        assert_eq!(app.current_flash(now), None);
        app.state.apply(Event::RunStarted, now);
        assert_eq!(app.on_key(ch('r'), now), Command::None);
        assert_eq!(app.current_flash(now), Some("a check is already running"));
        app.state.apply(Event::RunFinished, now);
        assert_eq!(app.on_key(ch('r'), now), Command::Refresh);
    }

    #[test]
    fn activation_confirm_runs_exactly_the_shown_command() {
        let (mut app, now) = mixed_app();
        select(&mut app, "al2-aarch64");
        let expected = app.activation(0).expect("activation");
        assert_eq!(app.on_key(ch('a'), now), Command::None);
        let Mode::Confirm(confirm) = app.mode.clone() else {
            panic!("expected a confirmation, got {:?}", app.mode);
        };
        assert_eq!(confirm.id, 0);
        assert_eq!(confirm.title, "Activate al2-aarch64?");
        assert_eq!(confirm.command, expected.command);
        assert!(
            confirm.command.contains(REV),
            "uses the locked flake: {}",
            confirm.command
        );
        assert!(
            confirm
                .command
                .contains(&path('b', "home-manager-generation").to_string())
        );
        assert_eq!(
            confirm.runs_on,
            "on al2-aarch64 (devhost-devuser1-2c-2b7c5f39.dc-west2.example.com)"
        );
        assert_eq!(confirm.warning, None);
        assert_eq!(confirm.after, After::Reprobe);
        assert!(!confirm.from_diff);
        // Other keys don't confirm.
        for k in [ch('j'), ch('a'), ch('Y'), ctrl('y'), key(KeyCode::Tab)] {
            assert_eq!(app.on_key(k, now), Command::None);
            assert!(matches!(app.mode, Mode::Confirm(_)));
        }
        assert_eq!(
            app.on_key(ch('y'), now),
            Command::Run {
                id: 0,
                command: confirm.command,
                after: After::Reprobe
            }
        );
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn only_y_confirms_and_n_esc_q_cancel() {
        let (mut app, now) = mixed_app();
        select(&mut app, "bigmem");
        for cancel in [ch('n'), key(KeyCode::Esc), ch('q')] {
            app.on_key(ch('a'), now);
            assert!(matches!(app.mode, Mode::Confirm(_)));
            assert_eq!(app.on_key(cancel, now), Command::None);
            assert_eq!(app.mode, Mode::Normal);
        }
        app.on_key(ch('a'), now);
        let Mode::Confirm(confirm) = app.mode.clone() else {
            panic!("expected a confirmation");
        };
        assert_eq!(confirm.runs_on, "here");
        assert!(
            confirm.command.contains(HM_9),
            "home-manager checks the out path"
        );
        // Enter opens the diff a screen earlier: here it only says what
        // the keys are.
        assert_eq!(app.on_key(key(KeyCode::Enter), now), Command::None);
        assert!(matches!(app.mode, Mode::Confirm(_)));
        assert_eq!(app.current_flash(now), Some("y runs it, n or esc cancels"));
        assert!(matches!(
            app.on_key(ch('y'), now),
            Command::Run {
                id: 3,
                after: After::Reprobe,
                ..
            }
        ));
    }

    #[test]
    fn a_confirmation_too_long_for_its_box_scrolls() {
        let (mut app, now) = mixed_app();
        select(&mut app, "al2-aarch64");
        app.on_key(ch('a'), now);
        let scroll = |app: &App| match &app.mode {
            Mode::Confirm(confirm) => confirm.scroll,
            other => panic!("left the confirmation: {other:?}"),
        };
        assert_eq!(scroll(&app), 0);
        // What the box measured: 3 of the command's lines at a time, 4 more.
        app.geometry().confirm_rows.store(3, Ordering::Relaxed);
        app.geometry().confirm_max.store(4, Ordering::Relaxed);
        for (k, want) in [
            (ch('j'), 1),
            (key(KeyCode::Down), 2),
            (key(KeyCode::End), 4),
            (ch('j'), 4),
            (ch('k'), 3),
            (key(KeyCode::PageUp), 0),
            (ch(' '), 3),
            (key(KeyCode::Home), 0),
            (key(KeyCode::PageDown), 3),
            (key(KeyCode::Up), 2),
            (ch('G'), 4),
            (ch('g'), 0),
        ] {
            assert_eq!(app.on_key(k, now), Command::None, "{k:?}");
            assert_eq!(scroll(&app), want, "{k:?}");
        }
        assert!(matches!(
            app.on_key(ch('y'), now),
            Command::Run { id: 0, .. }
        ));
        // A new confirmation starts at the top.
        app.on_key(ch('a'), now);
        assert_eq!(scroll(&app), 0);
    }

    #[test]
    fn activation_is_refused_on_busy_hosts_and_without_a_desired_generation() {
        let (mut app, now) = mixed_app();
        select(&mut app, "al2023-x86_64");
        app.on_key(ch('a'), now);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.current_flash(now),
            Some("al2023-x86_64 is busy (evaluating)")
        );

        select(&mut app, "desk");
        app.on_key(ch('a'), now);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.current_flash(now),
            Some("can't activate desk: its configuration doesn't evaluate")
        );

        // The forced-command route: the action's own reason.
        select(&mut app, "mac-m1");
        let why = app
            .activation(6)
            .expect_err("forced-command routes can't activate");
        app.on_key(ch('a'), now);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(why, "mac-m1 only answers its probe command");
        assert_eq!(
            app.current_flash(now),
            Some("can't activate: mac-m1 only answers its probe command")
        );

        // A host whose diff runs is busy too.
        let later = now + Duration::from_secs(1);
        select(&mut app, "bigmem");
        let (current, desired) = (store(HM_1), store(HM_9));
        app.state.apply(
            Event::DiffStarted {
                id: 3,
                current,
                desired,
            },
            later,
        );
        app.on_key(ch('a'), later);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.current_flash(later), Some("bigmem is busy (diffing)"));
    }

    #[test]
    fn activation_reasons() {
        let mut s = Script::new(vec![ssh_host("a", Kind::HomeManager, "a")]);
        let app = App::new(s.state.clone(), false);
        assert_eq!(
            app.activation(0),
            Err("desired generation not known yet".into())
        );
        assert_eq!(app.activation(7), Err("no such host".into()));
        s.apply(0, Event::EvalStarted(0));
        let app = App::new(s.state.clone(), false);
        assert_eq!(
            app.activation(0),
            Err("desired generation not known yet (evaluating)".into())
        );
        s.evaluated(0, 10, desired(path('a', "home-manager-generation"), false));
        let app = App::new(s.state.clone(), false);
        assert_eq!(app.activation(0), Err("its flake isn't resolved".into()));
        s.flake(20);
        let app = App::new(s.state.clone(), false);
        let activation = app.activation(0).expect("activation");
        assert_eq!(activation.runs_on, RunsOn::Remote);
    }

    #[test]
    fn activating_an_up_to_date_host_warns() {
        let (mut app, now) = mixed_app();
        select(&mut app, "web4");
        app.on_key(ch('a'), now);
        let Mode::Confirm(confirm) = &app.mode else {
            panic!("expected a confirmation");
        };
        assert_eq!(
            confirm.warning.as_deref(),
            Some("web4 is already up to date: this changes nothing")
        );
        assert_eq!(confirm.runs_on, "on web4");
        assert_eq!(
            join_warnings(Some("a".into()), Some("b".into())),
            Some("a; b".into())
        );
        assert_eq!(join_warnings(None, Some("b".into())), Some("b".into()));
        assert_eq!(join_warnings(None, None), None);
    }

    #[test]
    fn copy_is_paste_safe() {
        let (mut app, now) = mixed_app();
        select(&mut app, "al2-aarch64");
        assert_eq!(
            app.on_key(ch('y'), now),
            Command::Copy("hostctl activate -- al2-aarch64".into())
        );
        assert_eq!(
            app.current_flash(now),
            Some("copied: hostctl activate -- al2-aarch64")
        );
        // With the flags hostctl was started with.
        app.state.command = "hostctl -c /home/jj/hosts.toml".into();
        assert_eq!(
            app.on_key(ch('y'), now),
            Command::Copy("hostctl -c /home/jj/hosts.toml activate -- al2-aarch64".into())
        );
        select(&mut app, "bigmem");
        let Command::Copy(text) = app.on_key(ch('y'), now) else {
            panic!("expected a copy");
        };
        let activation = app.activation(3).unwrap();
        assert_eq!(activation.runs_on, RunsOn::Here);
        assert_eq!(
            text,
            actions::copy_text(&app.state.hosts[3].host, &activation, "hostctl").unwrap(),
            "local: the command itself, ready to paste"
        );
        // Copying is allowed on busy hosts but needs an activation.
        select(&mut app, "desk");
        assert_eq!(app.on_key(ch('y'), now), Command::None);
        assert_eq!(
            app.current_flash(now),
            Some("nothing to copy for desk: its configuration doesn't evaluate")
        );
        // Nothing a pasting terminal would act on reaches the clipboard.
        app.state.hosts[3].host.activate = Some("home-manager switch\x1b]52;c;eA==\x07".into());
        select(&mut app, "bigmem");
        assert_eq!(app.on_key(ch('y'), now), Command::None);
        assert_eq!(
            app.current_flash(now),
            Some("nothing to copy for bigmem: the command contains terminal control characters")
        );
    }

    #[test]
    fn diff_view_opens_and_starts_a_diff_once() {
        let (mut app, now) = mixed_app();
        select(&mut app, "bigmem");
        assert_eq!(app.on_key(ch('d'), now), Command::Diff(3));
        assert_eq!(app.mode, Mode::Diff { id: 3, scroll: 0 });
        assert_eq!(app.on_key(ch('q'), now), Command::None);
        assert_eq!(app.mode, Mode::Normal);
        // Running: Enter just opens the view.
        let (current, desired) = (store(HM_1), store(HM_9));
        app.state.apply(
            Event::DiffStarted {
                id: 3,
                current: current.clone(),
                desired: desired.clone(),
            },
            now,
        );
        assert_eq!(app.on_key(key(KeyCode::Enter), now), Command::None);
        assert_eq!(app.mode, Mode::Diff { id: 3, scroll: 0 });
        app.on_key(key(KeyCode::Esc), now);
        // Done: still nothing to start.
        app.state.apply(
            Event::DiffFinished {
                id: 3,
                current: current.clone(),
                desired: desired.clone(),
                result: Ok(DiffOutcome::Diff {
                    diff: fixture_diff(),
                }),
            },
            now,
        );
        assert_eq!(app.on_key(ch('d'), now), Command::None);
        app.on_key(ch('q'), now);
        // A failed diff is retried.
        app.state.apply(
            Event::DiffStarted {
                id: 3,
                current: current.clone(),
                desired: desired.clone(),
            },
            now,
        );
        app.state.apply(
            Event::DiffFinished {
                id: 3,
                current,
                desired,
                result: Err(ErrorInfo::msg("nix-store: command not found")),
            },
            now,
        );
        assert_eq!(app.on_key(ch('d'), now), Command::Diff(3));
    }

    #[test]
    fn diff_view_is_refused_without_two_different_generations() {
        let (mut app, now) = mixed_app();
        let cases = [
            ("web4", "web4 is up to date: nothing to diff"),
            (
                "al2-x86_64",
                "al2-x86_64 has no generation to compare: no home-manager profile",
            ),
            ("pi", "pi's current generation is unknown (unreachable)"),
            ("desk", "desk's configuration doesn't evaluate"),
            ("al2023-x86_64", "al2023-x86_64 is busy (evaluating)"),
        ];
        for (name, why) in cases {
            select(&mut app, name);
            assert_eq!(app.on_key(ch('d'), now), Command::None, "{name}");
            assert_eq!(app.mode, Mode::Normal, "{name}");
            assert_eq!(app.current_flash(now), Some(why), "{name}");
        }
        let fresh = Script::new(vec![ssh_host("a", Kind::Nixos, "a")]);
        assert_eq!(
            diff_problem(&fresh.state.hosts[0]).as_deref(),
            Some("a hasn't been probed yet")
        );
    }

    #[test]
    fn diff_view_scrolls_within_the_diff() {
        let mut s = mixed();
        s.diffed(
            3,
            500,
            Ok(DiffOutcome::Diff {
                diff: fixture_diff(),
            }),
        );
        let now = s.at(20_000);
        let mut app = App::new(s.state, true);
        select(&mut app, "bigmem");
        assert_eq!(app.on_key(ch('d'), now), Command::None, "already diffed");
        let scroll = |app: &App| match app.mode {
            Mode::Diff { scroll, .. } => scroll,
            _ => panic!("left the diff view: {:?}", app.mode),
        };
        // Unmeasured: one change per row (6 changes → at most 5).
        press(&mut app, &[ch('j'), ch('j'), key(KeyCode::Down)], now);
        assert_eq!(scroll(&app), 3);
        app.on_key(ch('k'), now);
        assert_eq!(scroll(&app), 2);
        app.on_key(key(KeyCode::Up), now);
        app.on_key(key(KeyCode::Up), now);
        app.on_key(key(KeyCode::Up), now);
        assert_eq!(scroll(&app), 0);
        app.on_key(ch('G'), now);
        assert_eq!(scroll(&app), 5);
        app.on_key(ch('g'), now);
        assert_eq!(scroll(&app), 0);
        app.on_key(key(KeyCode::PageDown), now);
        assert_eq!(scroll(&app), 5);
        // Measured by a frame: its limit and page size apply.
        app.geometry().diff_max.store(2, Ordering::Relaxed);
        app.geometry().diff_rows.store(4, Ordering::Relaxed);
        app.on_key(key(KeyCode::End), now);
        assert_eq!(scroll(&app), 2);
        app.on_key(key(KeyCode::PageUp), now);
        assert_eq!(scroll(&app), 0);
        app.on_key(ch(' '), now);
        assert_eq!(scroll(&app), 2);
        app.on_key(key(KeyCode::Home), now);
        assert_eq!(scroll(&app), 0);
        // Unknown keys and chords are ignored; Esc goes back.
        assert_eq!(app.on_key(ch('x'), now), Command::None);
        assert_eq!(app.on_key(ctrl('j'), now), Command::None);
        assert_eq!(scroll(&app), 0);
        assert_eq!(app.on_key(key(KeyCode::Esc), now), Command::None);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn diff_view_rediffs_on_r_unless_running() {
        let mut s = mixed();
        s.diffed(3, 500, Err(ErrorInfo::msg("connection reset")));
        let now = s.at(20_000);
        let mut app = App::new(s.state, true);
        app.mode = Mode::Diff { id: 3, scroll: 0 };
        assert_eq!(app.on_key(ch('r'), now), Command::Diff(3));
        app.state.apply(
            Event::DiffStarted {
                id: 3,
                current: store(HM_1),
                desired: store(HM_9),
            },
            now,
        );
        assert_eq!(app.on_key(ch('r'), now), Command::None);
        assert_eq!(
            app.current_flash(now),
            Some("a diff of bigmem is already running")
        );
        assert_eq!(app.mode, Mode::Diff { id: 3, scroll: 0 });
    }

    #[test]
    fn build_is_offered_only_when_the_desired_generation_is_missing() {
        let mut s = mixed();
        s.diffed(
            3,
            500,
            Ok(DiffOutcome::Diff {
                diff: fixture_diff(),
            }),
        );
        s.diffed(0, 700, Ok(DiffOutcome::DesiredMissing));
        let now = s.at(20_000);
        let mut app = App::new(s.state, true);

        app.mode = Mode::Diff { id: 3, scroll: 1 };
        assert_eq!(app.on_key(ch('b'), now), Command::None);
        assert_eq!(app.mode, Mode::Diff { id: 3, scroll: 1 });
        assert_eq!(
            app.current_flash(now),
            Some("b builds the new generation when it is missing")
        );

        app.mode = Mode::Diff { id: 0, scroll: 0 };
        app.on_key(ch('b'), now);
        let Mode::Confirm(confirm) = app.mode.clone() else {
            panic!("expected a build confirmation, got {:?}", app.mode);
        };
        assert_eq!(confirm.title, "Build the new generation on al2-aarch64?");
        let host = &app.state.hosts[0].host;
        assert_eq!(
            Ok(confirm.command.clone()),
            actions::build_command(host, &flake_info())
        );
        assert!(
            confirm.command.contains(REV),
            "builds the compared revision"
        );
        assert_eq!(
            confirm.runs_on,
            "on al2-aarch64 (devhost-devuser1-2c-2b7c5f39.dc-west2.example.com)"
        );
        assert_eq!(confirm.after, After::Rediff);
        assert!(confirm.from_diff);
        assert_eq!(app.diff_scroll(), 0);
        // Cancelling returns to the diff view.
        assert_eq!(app.on_key(ch('n'), now), Command::None);
        assert_eq!(app.mode, Mode::Diff { id: 0, scroll: 0 });
        // Confirming runs it and stays in the diff view for the new diff.
        app.on_key(ch('b'), now);
        assert_eq!(
            app.on_key(ch('y'), now),
            Command::Run {
                id: 0,
                command: confirm.command,
                after: After::Rediff
            }
        );
        assert_eq!(app.mode, Mode::Diff { id: 0, scroll: 0 });
    }

    #[test]
    fn build_is_refused_where_it_cant_run() {
        let mut s = mixed();
        // The forced-command Mac: reachable for a diff from the local store only.
        s.state.hosts[6].probe = s.state.hosts[3].probe.clone();
        s.diffed(6, 500, Ok(DiffOutcome::DesiredMissing));
        let now = s.at(20_000);
        let mut app = App::new(s.state, true);
        let why = actions::build_command(&app.state.hosts[6].host, &flake_info())
            .expect_err("forced-command routes can't build");
        app.mode = Mode::Diff { id: 6, scroll: 0 };
        app.on_key(ch('b'), now);
        assert_eq!(app.mode, Mode::Diff { id: 6, scroll: 0 });
        assert_eq!(
            app.current_flash(now),
            Some(format!("can't build: {why}").as_str()),
            "the reason names the host"
        );
    }

    #[test]
    fn activation_from_the_diff_view_returns_to_it() {
        let mut s = mixed();
        s.diffed(
            3,
            500,
            Ok(DiffOutcome::Diff {
                diff: fixture_diff(),
            }),
        );
        let now = s.at(20_000);
        let mut app = App::new(s.state, true);
        app.mode = Mode::Diff { id: 3, scroll: 2 };
        app.on_key(ch('a'), now);
        let Mode::Confirm(confirm) = app.mode.clone() else {
            panic!("expected a confirmation");
        };
        assert!(confirm.from_diff);
        assert_eq!(confirm.after, After::Reprobe);
        assert_eq!(app.diff_scroll(), 2, "the diff stays visible underneath");
        app.on_key(key(KeyCode::Esc), now);
        assert_eq!(app.mode, Mode::Diff { id: 3, scroll: 2 }, "scroll restored");
        app.on_key(ch('a'), now);
        assert!(matches!(
            app.on_key(ch('y'), now),
            Command::Run { id: 3, .. }
        ));
        assert_eq!(
            app.mode,
            Mode::Normal,
            "the diff is obsolete after activating"
        );
        // `y` in the diff view copies.
        app.mode = Mode::Diff { id: 3, scroll: 0 };
        assert!(matches!(app.on_key(ch('y'), now), Command::Copy(_)));
    }

    #[test]
    fn next_wakeup_cases() {
        let (mut app, now) = mixed_app();
        // Host 2 still evaluates: spinner frames.
        assert_eq!(app.next_wakeup(now, None), Some(now + TICK));
        app.state.apply(Event::Interrupted, now);
        assert!(!app.state.is_busy());
        // Idle: age labels every 30 s.
        assert_eq!(
            app.next_wakeup(now, None),
            Some(now + Duration::from_secs(30))
        );
        // A refresh timer that comes first.
        let soon = now + Duration::from_secs(5);
        assert_eq!(app.next_wakeup(now, Some(soon)), Some(soon));
        let late = now + Duration::from_secs(600);
        assert_eq!(
            app.next_wakeup(now, Some(late)),
            Some(now + Duration::from_secs(30))
        );
        // A due refresh is due now.
        assert_eq!(app.next_wakeup(now, Some(now)), Some(now));
        // The flash's expiry; an expired flash no longer matters.
        app.flash("hi", now - Duration::from_secs(1));
        assert_eq!(
            app.next_wakeup(now, None),
            Some(now + Duration::from_secs(2))
        );
        app.flash("hi", now - FLASH_FOR);
        assert_eq!(app.current_flash(now), None);
        assert_eq!(
            app.next_wakeup(now, None),
            Some(now + Duration::from_secs(30))
        );
        // A running diff (no check) still spins.
        app.state.apply(
            Event::DiffStarted {
                id: 3,
                current: store(HM_1),
                desired: store(HM_9),
            },
            now,
        );
        assert_eq!(app.next_wakeup(now, Some(soon)), Some(now + TICK));
        // A running check with nothing started yet spins too.
        let mut idle = App::new(AppState::default(), false);
        assert_eq!(
            idle.next_wakeup(now, None),
            Some(now + Duration::from_secs(30))
        );
        idle.state.apply(Event::RunStarted, now);
        assert_eq!(idle.next_wakeup(now, None), Some(now + TICK));
    }

    #[test]
    fn flashes_expire() {
        let (mut app, now) = mixed_app();
        assert_eq!(app.current_flash(now), None);
        app.flash("copied", now);
        assert_eq!(
            app.current_flash(now + Duration::from_millis(2999)),
            Some("copied")
        );
        assert_eq!(app.current_flash(now + FLASH_FOR), None);
        app.tick();
        app.tick();
        assert_eq!(app.spinner, 2);
        app.spinner = usize::MAX;
        app.tick();
        assert_eq!(app.spinner, 0);
    }

    #[test]
    fn interrupted_probes_leave_hosts_pending() {
        let mut s = Script::new(vec![ssh_host("a", Kind::Nixos, "a")]);
        s.probe_failed(
            0,
            10,
            ProbeFailure::Interrupted,
            ErrorInfo::msg("interrupted"),
        );
        let mut app = App::new(s.state, false);
        let now = Instant::now();
        assert_eq!(app.on_key(ch('d'), now), Command::None);
        assert_eq!(
            app.current_flash(now),
            Some("a's current generation is unknown (pending)")
        );
    }

    #[test]
    fn refusals_name_the_host_once() {
        let flake_only_here = "this flake only exists here: use activate-on = \"local\"";
        let cases = [
            (
                "can't activate",
                "",
                "desk",
                "its configuration doesn't evaluate",
                "can't activate desk: its configuration doesn't evaluate".to_string(),
            ),
            (
                "can't activate",
                "",
                "mac",
                "activation is disabled for mac (activate = false)",
                "can't activate: activation is disabled for mac (activate = false)".to_string(),
            ),
            (
                "can't build",
                "for",
                "a",
                "a only answers its probe command",
                "can't build: a only answers its probe command".to_string(),
            ),
            (
                "can't build",
                "for",
                "a",
                flake_only_here,
                format!("can't build for a: {flake_only_here}"),
            ),
            (
                "nothing to copy",
                "for",
                "jj@mac",
                "'jj@mac' is unknown",
                "nothing to copy: 'jj@mac' is unknown".to_string(),
            ),
        ];
        for (what, by, name, why, want) in cases {
            assert_eq!(refusal(what, by, name, why), want);
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(step(0, -1, 5), 0);
        assert_eq!(step(4, 3, 5), 5);
        assert_eq!(step(u16::MAX - 1, 5, UNKNOWN), u16::MAX);
        assert_eq!(page(0), DEFAULT_PAGE);
        assert_eq!(page(7), 7);
        let host = ssh_host("web1", Kind::Nixos, "web1");
        assert_eq!(runs_on_label(&host, RunsOn::Remote), "on web1");
        assert_eq!(runs_on_label(&host, RunsOn::Here), "here");
        let host = ssh_host("web1", Kind::Nixos, "root@web1:2222");
        assert_eq!(
            runs_on_label(&host, RunsOn::Remote),
            "on web1 (root@web1:2222)"
        );
    }
}
