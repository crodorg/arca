//! TUI app state and view rendering.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Bar, BarChart, BarGroup, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row,
    Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
};

use arca_core::money::Cents;
use arca_core::rpc::{
    AlertsPage, BusinessSnapshot, CategoriesSnapshot, CategorySpend, ChartsSnapshot, DebtSnapshot,
    MoneySnapshot, MonthFlow, PpSnapshot, RecurringPage, Request, Response, Scope, TimePoint,
    TxListPage,
};
use arca_core::time::{display_ast, display_date, display_short_date};

use crate::client::{Transport, call};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Menu, // the home launcher (summary bar + destination list + recent txns)
    Money,
    Business,
    Pp,
    Bills, // composite: debt + recurring (with :label)
    Tx,
    Alerts,
    Charts,
    Expenses,
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Normal,
    Cmd,    // `:`
    Search, // `/`
}

/// Which list the Bills cursor acts on: the unconfirmed triage inbox, or the
/// confirmed bills that graduated into the scheduled pane. `Tab` flips between
/// them so the action menu can reach an already-labeled bill too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BillsZone {
    Triage,
    Schedule,
    /// Series the operator hid with Ignore — surfaced only here so they can be
    /// restored (the daemon sends them in `RecurringPage.ignored`).
    Ignored,
}

/// A Bills action-menu item and the mutation it maps to. Detected-series actions
/// go through `recurring.confirm`; the two `Sub*` actions edit a declared
/// subscription via `manual.update_subscription`.
#[derive(Clone, Copy, Debug)]
enum BillsAction {
    /// Set the treatment label: `"sub"` | `"bill"` | `"debt"`.
    Label(&'static str),
    /// Set a friendly display name — drops into Cmd-mode input (the name has to be
    /// typed; it's invented, not a fixed keyword).
    Rename,
    /// Soft-dismiss the current label (only offered when one is set).
    Unlabel,
    /// Hide as a false positive.
    Ignore,
    /// Declared subscription: rename it (changes its name — drops into Cmd input).
    SubRename,
    /// Declared subscription: deactivate it (drop it from the schedule).
    SubRemove,
}

/// What a row in the scheduled pane *is*, so the Schedule-zone cursor knows what
/// `Enter` can do there. The cursor walks every scheduled row; `Detected` opens the
/// recurring menu, `Declared` opens a Rename/Remove menu, `Debt` is informational.
#[derive(Clone, Debug)]
enum SchedKind {
    Detected { key: String, label: String },
    Declared { name: String },
    Debt,
}

/// One row of the Bills scheduled pane, shared by the renderer and the
/// Schedule-zone cursor so they index the same list (sorted by due date).
#[derive(Clone, Debug)]
struct SchedRow {
    due: i64,
    desc: String,
    amount: String,
    tag: String,
    forecast: bool,
    kind: SchedKind,
}

/// The open Bills action popup: the target (series payee or subscription name)
/// plus its menu items and cursor. `None` on `App` means closed.
pub struct BillsMenu {
    match_key: String,
    label: Option<String>,
    title: String,
    items: Vec<(BillsAction, &'static str)>,
    sel: usize,
}

pub struct Connection {
    pub socket: Option<PathBuf>,
    pub tcp: Option<String>,
}

impl Connection {
    pub fn transport(&self) -> Transport<'_> {
        if let Some(p) = &self.socket {
            Transport::Unix(p)
        } else if let Some(t) = &self.tcp {
            Transport::Tcp(t.as_str())
        } else {
            panic!("connection has no transport");
        }
    }
}

#[derive(Default)]
pub struct ViewData {
    pub money: Option<MoneySnapshot>,
    pub business: Option<BusinessSnapshot>,
    pub pp: Option<PpSnapshot>,
    pub debt: Option<DebtSnapshot>,
    pub tx: Option<TxListPage>,
    pub recurring: Option<RecurringPage>,
    pub alerts: Option<AlertsPage>,
    pub charts: Option<ChartsSnapshot>,
    pub categories: Option<CategoriesSnapshot>,
}

pub struct App {
    pub view: View,
    pub mode: Mode,
    pub buf: String,          // current : or / buffer
    pub business_tag: String, // current biz tag (default "main")
    pub conn: Connection,
    pub data: ViewData,
    pub status: String,
    pub status_is_err: bool, // colors the status line red vs green
    pub quit: bool,
    pub last_refresh: Instant,
    pub scroll: usize,                 // first visible row/line of the current view
    pub filter: String,                // active `/` filter (empty = none); list views only
    pub pending_g: bool,               // first `g` of a `gg` (jump-to-top) chord
    pub content_len: usize,            // total rows/lines, set during render (for clamping)
    pub viewport: usize,               // visible body height, set during render
    pub charts_offset: usize,          // Charts view: months scrolled back from newest ([/])
    pub menu_sel: usize,               // Main-menu cursor (index into MENU_ITEMS); j/k move it
    pub bills_sel: usize,              // Bills cursor (index into the active zone's rows)
    pub bills_zone: BillsZone,         // which Bills list the cursor acts on (Tab flips it)
    pub bills_menu: Option<BillsMenu>, // open Bills action popup (None = closed)
}

/// The Main-menu destinations, in display order: `(key, label, target view)`. The
/// menu cursor (`menu_sel`) indexes this; the same letters jump straight to a view
/// from anywhere. Expenses (`e`) is reachable but not a tile — it folds into
/// Charts later.
const MENU_ITEMS: [(char, &str, View); 7] = [
    ('m', "Accounts", View::Money),
    ('t', "Transactions", View::Tx),
    ('d', "Bills", View::Bills),
    ('p', "Invest", View::Pp),
    ('v', "Charts", View::Charts),
    ('a', "Alerts", View::Alerts),
    ('b', "Business", View::Business),
];

impl App {
    pub fn new(conn: Connection) -> Self {
        Self {
            view: View::Menu,
            mode: Mode::Normal,
            buf: String::new(),
            business_tag: "main".into(),
            conn,
            data: ViewData::default(),
            status: String::new(),
            status_is_err: false,
            quit: false,
            last_refresh: Instant::now() - Duration::from_secs(3600),
            scroll: 0,
            filter: String::new(),
            pending_g: false,
            content_len: 0,
            viewport: 0,
            charts_offset: 0,
            menu_sel: 0,
            bills_sel: 0,
            bills_zone: BillsZone::Triage,
            bills_menu: None,
        }
    }

    /// Move the Main-menu cursor by `delta` rows, clamped to the destination list
    /// (no wrap). `j`/`k` and arrows drive it; `Enter` opens the selected view.
    fn menu_move(&mut self, delta: isize) {
        let last = (MENU_ITEMS.len() - 1) as isize;
        self.menu_sel = (self.menu_sel as isize + delta).clamp(0, last) as usize;
    }

    fn max_scroll(&self) -> usize {
        self.content_len.saturating_sub(self.viewport)
    }

    /// Move the (deep-view) scroll offset by `delta` lines, clamped to bounds. The
    /// Overview grid doesn't scroll — fullscreen a box to scroll it.
    fn scroll_by(&mut self, delta: isize) {
        let max = self.max_scroll() as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }

    fn jump_top(&mut self) {
        self.scroll = 0;
    }

    fn jump_bottom(&mut self) {
        self.scroll = self.max_scroll();
    }

    /// Half a viewport, used by Ctrl-u / Ctrl-d. At least one line.
    fn half_page(&self) -> isize {
        (self.viewport / 2).max(1) as isize
    }

    /// A full viewport, used by PageUp / PageDown. At least one line.
    fn page(&self) -> isize {
        self.viewport.max(1) as isize
    }

    pub async fn refresh_current(&mut self) {
        // The Menu and the Bills view each compose several snapshots; fetch those
        // in turn (the client is one-request-per-connection) rather than via the
        // single-request path below.
        match self.view {
            View::Menu => {
                self.refresh_menu().await;
                self.last_refresh = Instant::now();
                self.scroll = 0;
                return;
            }
            View::Bills => {
                self.refresh_bills().await;
                self.last_refresh = Instant::now();
                self.scroll = self.scroll.min(self.max_scroll());
                // Keep the cursor in range — a confirm/refresh may have shrunk the
                // active zone's set out from under it.
                let n = self.bills_zone_len(self.bills_zone);
                self.bills_sel = self.bills_sel.min(n.saturating_sub(1));
                return;
            }
            View::Charts => {
                self.refresh_charts().await;
                self.last_refresh = Instant::now();
                self.scroll = 0;
                return;
            }
            _ => {}
        }
        let req = match self.view {
            View::Menu | View::Bills | View::Charts => unreachable!("handled above"),
            View::Money => Request::SnapshotMoney,
            View::Business => Request::SnapshotBusiness {
                tag: self.business_tag.clone(),
                scope: Some(Scope::Ytd),
            },
            View::Pp => Request::SnapshotPp,
            View::Tx => Request::TxList {
                since: None,
                tag: None,
                limit: Some(100),
            },
            View::Alerts => Request::AlertPending {
                limit: Some(50),
                include_delivered: Some(true),
            },
            View::Expenses => Request::SnapshotCategories {
                scope: Some(Scope::Month),
                limit: Some(20),
            },
            View::Help => return,
        };
        // A successful load clears any stale error/status; only the failure arms
        // re-arm it (and flag it red for draw_status).
        self.status.clear();
        self.status_is_err = false;
        match call(self.conn.transport(), &req).await {
            Ok(Response::Money(m)) => self.data.money = Some(m),
            Ok(Response::Business(b)) => self.data.business = Some(b),
            Ok(Response::Pp(p)) => self.data.pp = Some(p),
            Ok(Response::Debt(d)) => self.data.debt = Some(d),
            Ok(Response::TxList(t)) => self.data.tx = Some(t),
            Ok(Response::Recurring(r)) => self.data.recurring = Some(r),
            Ok(Response::Alerts(a)) => self.data.alerts = Some(a),
            Ok(Response::Charts(c)) => self.data.charts = Some(c),
            Ok(Response::Categories(c)) => self.data.categories = Some(c),
            Ok(Response::Error(e)) => {
                self.status = format!("err: {}: {}", e.code, e.msg);
                self.status_is_err = true;
            }
            Ok(other) => {
                self.status = format!("unexpected response: {other:?}");
                self.status_is_err = true;
            }
            Err(e) => {
                self.status = format!("rpc: {e}");
                self.status_is_err = true;
            }
        }
        self.last_refresh = Instant::now();
        // Keep the reader's place on a manual `r` refresh; only clamp if the new
        // data is shorter. View switches reset to the top in `switch_view`.
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Fan out every snapshot the Main menu and the summary bar draw from (net
    /// worth, charts, debt, recurring, alerts, PP, business, recent txns), in turn
    /// — the client is one-request-per-connection. A failed leg arms the status
    /// line but doesn't abort the rest, so a partial menu still shows what loaded.
    /// These also seed the summary bar, which then persists (cached in `self.data`)
    /// across deep-view switches.
    async fn refresh_menu(&mut self) {
        self.status.clear();
        self.status_is_err = false;
        let reqs = [
            Request::SnapshotMoney,
            Request::SnapshotCharts { months: Some(12) },
            Request::SnapshotDebt {
                scope: Scope::Month,
            },
            Request::RecurringList {
                since: None,
                min_occurrences: None,
                include_ignored: None, // menu/summary never needs the ignored ones
            },
            Request::AlertPending {
                limit: Some(50),
                include_delivered: Some(true),
            },
            Request::SnapshotPp,
            Request::SnapshotBusiness {
                tag: self.business_tag.clone(),
                scope: Some(Scope::Month),
            },
            // Recent panel (fills its pane on a tall terminal) + the Tx teaser.
            Request::TxList {
                since: None,
                tag: None,
                limit: Some(40),
            },
        ];
        for req in reqs {
            match call(self.conn.transport(), &req).await {
                Ok(Response::Money(m)) => self.data.money = Some(m),
                Ok(Response::Charts(c)) => self.data.charts = Some(c),
                Ok(Response::Debt(d)) => self.data.debt = Some(d),
                Ok(Response::Recurring(r)) => self.data.recurring = Some(r),
                Ok(Response::Alerts(a)) => self.data.alerts = Some(a),
                Ok(Response::Pp(p)) => self.data.pp = Some(p),
                Ok(Response::Business(b)) => self.data.business = Some(b),
                Ok(Response::TxList(t)) => self.data.tx = Some(t),
                Ok(Response::Error(e)) => self.set_err(format!("err: {}: {}", e.code, e.msg)),
                Ok(other) => self.set_err(format!("unexpected response: {other:?}")),
                Err(e) => self.set_err(format!("rpc: {e}")),
            }
        }
    }

    /// Fetch the two snapshots the Bills deep view composes (debt + recurring).
    /// A failed leg arms the status line but doesn't abort the other.
    async fn refresh_bills(&mut self) {
        self.status.clear();
        self.status_is_err = false;
        let reqs = [
            Request::SnapshotDebt {
                scope: Scope::Month,
            },
            Request::RecurringList {
                since: None,
                min_occurrences: None,
                include_ignored: Some(true), // Bills needs the ignored zone populated
            },
        ];
        for req in reqs {
            match call(self.conn.transport(), &req).await {
                Ok(Response::Debt(d)) => self.data.debt = Some(d),
                Ok(Response::Recurring(r)) => self.data.recurring = Some(r),
                Ok(Response::Error(e)) => self.set_err(format!("err: {}: {}", e.code, e.msg)),
                Ok(other) => self.set_err(format!("unexpected response: {other:?}")),
                Err(e) => self.set_err(format!("rpc: {e}")),
            }
        }
    }

    /// Charts is a composite (trend + cash flow + category spend): fetch the
    /// charts snapshot (60-month history, windowed client-side by `[`/`]`) plus
    /// the month's category spend (folded in from the old Expenses view). A failed
    /// leg arms the status line but doesn't abort the other.
    async fn refresh_charts(&mut self) {
        self.status.clear();
        self.status_is_err = false;
        let reqs = [
            Request::SnapshotCharts { months: Some(60) },
            Request::SnapshotCategories {
                scope: Some(Scope::Month),
                limit: Some(20),
            },
        ];
        for req in reqs {
            match call(self.conn.transport(), &req).await {
                Ok(Response::Charts(c)) => self.data.charts = Some(c),
                Ok(Response::Categories(c)) => self.data.categories = Some(c),
                Ok(Response::Error(e)) => self.set_err(format!("err: {}: {}", e.code, e.msg)),
                Ok(other) => self.set_err(format!("unexpected response: {other:?}")),
                Err(e) => self.set_err(format!("rpc: {e}")),
            }
        }
    }

    /// Set a green (success) status line.
    fn set_ok(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_err = false;
    }

    /// Set a red (failure) status line.
    fn set_err(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_err = true;
    }

    /// Charts view: slide the visible window. `delta` is in months — positive
    /// scrolls toward older history (h), negative toward newest (l). Offset is
    /// clamped to the fetched cash-flow length minus the window.
    fn charts_scroll(&mut self, delta: isize) {
        let n = self.data.charts.as_ref().map_or(0, |c| c.cash_flow.len());
        let win = CHART_WINDOW_MONTHS.min(n.max(1));
        let max_off = n.saturating_sub(win) as isize;
        let next = (self.charts_offset as isize + delta).clamp(0, max_off);
        self.charts_offset = next as usize;
    }

    pub async fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<()> {
        match self.mode {
            Mode::Cmd | Mode::Search => self.handle_input_mode(code).await?,
            Mode::Normal => self.handle_normal(code, mods).await?,
        }
        Ok(())
    }

    /// Switch to `view`, drop any active filter, refresh, and jump to the top.
    async fn switch_view(&mut self, view: View) {
        self.view = view;
        self.filter.clear();
        self.refresh_current().await;
        self.scroll = 0;
        self.charts_offset = 0;
        self.bills_sel = 0;
        self.bills_zone = BillsZone::Triage;
        self.bills_menu = None;
    }

    /// True on the Main menu (where j/k move the cursor and letters/Enter open a
    /// view); false in any view (where j/k scroll and Esc/h back to the menu).
    fn on_menu(&self) -> bool {
        self.view == View::Menu
    }

    async fn handle_normal(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<()> {
        // The Bills action menu, while open, captures every key (j/k/Enter/Esc).
        if self.bills_menu.is_some() {
            self.handle_bills_menu(code).await;
            return Ok(());
        }

        // `gg` chord: any key but a second `g` clears the pending first `g`.
        let was_pending_g = self.pending_g;
        self.pending_g = false;

        // Ctrl combos first, so Ctrl-d is half-page-down and not a letter verb.
        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('d') => self.scroll_by(self.half_page()),
                KeyCode::Char('u') => self.scroll_by(-self.half_page()),
                _ => {}
            }
            return Ok(());
        }

        // In the Bills view the cursor drives the detected-series lists: Tab flips
        // zone (triage inbox <-> confirmed bills), j/k pick, Enter opens the action
        // menu, 1/2/3/x quick-label. Handle it before the generic scroll/jump keys
        // so those act on the selection here.
        if self.view == View::Bills && self.handle_bills_keys(code).await? {
            return Ok(());
        }

        // j/k are modal: on the Main menu they move the cursor (Enter opens the
        // selection); in a view they scroll, and Esc/h backs out to the menu.
        if self.on_menu() {
            match code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.menu_move(1);
                    return Ok(());
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.menu_move(-1);
                    return Ok(());
                }
                KeyCode::Enter => {
                    let view = MENU_ITEMS[self.menu_sel.min(MENU_ITEMS.len() - 1)].2;
                    self.switch_view(view).await;
                    return Ok(());
                }
                _ => {}
            }
        } else {
            match code {
                // Back out of a view to the Main menu.
                KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc if self.filter.is_empty() => {
                    self.switch_view(View::Menu).await;
                    return Ok(());
                }
                // Esc with an active filter clears the filter first (a second Esc
                // then backs out, via the arm above).
                KeyCode::Esc => {
                    self.filter.clear();
                    return Ok(());
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.scroll_by(1);
                    return Ok(());
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.scroll_by(-1);
                    return Ok(());
                }
                // Charts view: [ / ] slide the month window (older/newer); { / } by
                // a page. (h is taken by back-out, so not h/l here.)
                KeyCode::Char('[') if self.view == View::Charts => self.charts_scroll(1),
                KeyCode::Char(']') if self.view == View::Charts => self.charts_scroll(-1),
                KeyCode::Char('{') if self.view == View::Charts => {
                    self.charts_scroll(CHART_WINDOW_MONTHS as isize);
                }
                KeyCode::Char('}') if self.view == View::Charts => {
                    self.charts_scroll(-(CHART_WINDOW_MONTHS as isize));
                }
                _ => {}
            }
        }

        match code {
            // q quits only from the menu; from a view it backs out, so you never
            // quit by accident mid-drilldown.
            KeyCode::Char('q') => {
                if self.on_menu() {
                    self.quit = true;
                } else {
                    self.switch_view(View::Menu).await;
                }
            }
            // Number keys open a menu destination directly (1-7 = the tile order).
            KeyCode::Char(n @ '1'..='7') => {
                let i = (n as usize - '1' as usize).min(MENU_ITEMS.len() - 1);
                self.menu_sel = i;
                self.switch_view(MENU_ITEMS[i].2).await;
            }
            // Mnemonic letters jump straight to a view from anywhere (the same
            // letters the menu lists); `o` returns to the menu home.
            KeyCode::Char('o') => self.switch_view(View::Menu).await,
            KeyCode::Char('m') => self.switch_view(View::Money).await,
            KeyCode::Char('b') => self.switch_view(View::Business).await,
            KeyCode::Char('p') => self.switch_view(View::Pp).await,
            KeyCode::Char('d' | 'c') => self.switch_view(View::Bills).await,
            KeyCode::Char('t') => self.switch_view(View::Tx).await,
            KeyCode::Char('a') => self.switch_view(View::Alerts).await,
            KeyCode::Char('v') => self.switch_view(View::Charts).await,
            KeyCode::Char('e') => self.switch_view(View::Expenses).await,
            KeyCode::Char('?') => self.switch_view(View::Help).await,
            KeyCode::Char('r') | KeyCode::F(5) => self.refresh_current().await,

            // Scrolling (deep views; the modal block above handled j/k/h already).
            KeyCode::PageDown => self.scroll_by(self.page()),
            KeyCode::PageUp => self.scroll_by(-self.page()),
            KeyCode::Char('G') | KeyCode::End => self.jump_bottom(),
            KeyCode::Home => self.jump_top(),
            KeyCode::Char('g') => {
                if was_pending_g {
                    self.jump_top(); // gg
                } else {
                    self.pending_g = true;
                }
            }

            KeyCode::Char(':') => {
                self.mode = Mode::Cmd;
                self.buf.clear();
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.buf.clear();
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_input_mode(&mut self, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.buf.clear();
            }
            KeyCode::Backspace => {
                self.buf.pop();
            }
            KeyCode::Enter => {
                let cmd = std::mem::take(&mut self.buf);
                let m = self.mode;
                self.mode = Mode::Normal;
                if m == Mode::Cmd {
                    self.run_cmd(&cmd).await;
                } else {
                    // `/` filter: case-insensitive substring over list views.
                    // Empty input clears the filter. Reset scroll to the top.
                    self.filter = cmd.trim().to_string();
                    self.scroll = 0;
                }
            }
            KeyCode::Tab if self.mode == Mode::Cmd => self.cmd_complete(),
            KeyCode::Char(c) => self.buf.push(c),
            _ => {}
        }
        Ok(())
    }

    /// TAB completion in Cmd (`:`) mode. With no space yet, complete the verb from
    /// the known list (unique → fill, several → extend to the common prefix). After
    /// a recurring verb + partial payee, complete the payee from the loaded series
    /// (unique substring match only — ambiguity leaves the buffer untouched).
    fn cmd_complete(&mut self) {
        const VERBS: &[&str] = &[
            "label ",
            "unlabel ",
            "rename ",
            "subrename ",
            "ignore ",
            "refresh ",
            "quit",
        ];
        let buf = self.buf.clone();
        let Some((verb, rest)) = buf.split_once(' ') else {
            // Still typing the verb.
            let hits: Vec<&&str> = VERBS.iter().filter(|v| v.starts_with(&buf)).collect();
            match hits.as_slice() {
                [one] => self.buf = (**one).to_string(),
                [] => {}
                many => {
                    let cp = common_prefix(&many.iter().map(|s| **s).collect::<Vec<_>>());
                    if cp.len() > buf.len() {
                        self.buf = cp;
                    }
                }
            }
            return;
        };
        // Payee completion for the recurring verbs. `:rename` takes `query = name`;
        // only complete the query side (before `=`).
        if !matches!(verb, "label" | "unlabel" | "rename" | "ignore") {
            return;
        }
        let partial = rest.split('=').next().unwrap_or(rest).trim();
        if partial.is_empty() {
            return;
        }
        let pl = partial.to_lowercase();
        let names: Vec<String> = self
            .data
            .recurring
            .as_ref()
            .map(|r| {
                r.series
                    .iter()
                    .map(|ls| {
                        ls.display_name
                            .clone()
                            .unwrap_or_else(|| ls.series.display.clone())
                    })
                    .filter(|n| n.to_lowercase().contains(&pl))
                    .collect()
            })
            .unwrap_or_default();
        if let [one] = names.as_slice() {
            self.buf = format!("{verb} {one}");
        }
    }

    async fn run_cmd(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        match cmd {
            "q" | "quit" => self.quit = true,
            "refresh" => self.refresh_current().await,
            _ if cmd.starts_with("refresh ") => {
                let kind = cmd["refresh ".len()..].trim().to_string();
                match call(
                    self.conn.transport(),
                    &Request::ProviderRefresh {
                        kind_filter: Some(kind.clone()),
                    },
                )
                .await
                {
                    Ok(Response::RefreshReport(r)) => {
                        self.set_ok(format!(
                            "refresh {kind}: {} rows{}",
                            r.rows_written,
                            r.message
                                .as_deref()
                                .map(|m| format!(" - {m}"))
                                .unwrap_or_default()
                        ));
                    }
                    Ok(other) => self.set_err(format!("unexpected: {other:?}")),
                    Err(e) => self.set_err(format!("rpc: {e}")),
                }
            }
            _ if cmd.starts_with("set tag=") => {
                self.business_tag = cmd["set tag=".len()..].trim().to_string();
                if matches!(self.view, View::Business) {
                    self.refresh_current().await;
                }
            }
            _ if cmd.starts_with("label ") => self.cmd_label(&cmd["label ".len()..]).await,
            _ if cmd.starts_with("unlabel ") => self.cmd_unlabel(&cmd["unlabel ".len()..]).await,
            _ if cmd.starts_with("subrename ") => {
                self.cmd_subrename(&cmd["subrename ".len()..]).await;
            }
            _ if cmd.starts_with("rename ") => self.cmd_rename(&cmd["rename ".len()..]).await,
            _ if cmd.starts_with("ignore ") => self.cmd_ignore(&cmd["ignore ".len()..]).await,
            _ => self.set_err(format!("unknown command: {cmd}")),
        }
    }

    /// `:label <payee query> <sub|bill|debt>` — confirm a detected series. The
    /// query is matched (case-insensitive substring) against the loaded recurring
    /// list; the trailing token is the label.
    async fn cmd_label(&mut self, args: &str) {
        let Some((query, label)) = args.trim().rsplit_once(char::is_whitespace) else {
            self.set_err("usage: :label <payee query> <sub|bill|debt>");
            return;
        };
        let label = label.trim();
        if !matches!(label, "sub" | "bill" | "debt") {
            self.set_err(format!("bad label {label:?}; use sub|bill|debt"));
            return;
        }
        match self.resolve_payee(query) {
            Ok((key, _)) => self.send_confirm(&key, Some(label), None, true).await,
            Err(e) => self.set_err(e),
        }
    }

    /// `:unlabel <payee query>` — soft-dismiss a previously set label. Carries the
    /// series' current label through with `active=false` (the verb requires one).
    async fn cmd_unlabel(&mut self, query: &str) {
        match self.resolve_payee(query) {
            Ok((_, None)) => self.set_err("series is not labeled"),
            Ok((key, Some(label))) => self.send_confirm(&key, Some(&label), None, false).await,
            Err(e) => self.set_err(e),
        }
    }

    /// `:rename <payee query> = <new name>` — set a friendly display name on any
    /// detected series, labeled or not (the label column is nullable; a rename
    /// preserves an existing label and leaves an unlabeled series unlabeled). The
    /// `=` separates a multi-word query from a multi-word name unambiguously.
    async fn cmd_rename(&mut self, args: &str) {
        let Some((query, name)) = args.split_once('=') else {
            self.set_err("usage: :rename <payee query> = <new name>");
            return;
        };
        let name = name.trim();
        if name.is_empty() {
            self.set_err("usage: :rename <payee query> = <new name>");
            return;
        }
        match self.resolve_payee(query) {
            // Pass no label: a NULL label preserves any existing one server-side.
            Ok((key, _)) => self.send_confirm(&key, None, Some(name), true).await,
            Err(e) => self.set_err(e),
        }
    }

    /// `:subrename <old name> = <new name>` — rename a *declared* subscription (the
    /// Schedule zone's Rename action prefills this). Exact name match, not a payee
    /// query — declared subs aren't detected series.
    async fn cmd_subrename(&mut self, args: &str) {
        let Some((old, new)) = args.split_once('=') else {
            self.set_err("usage: :subrename <old name> = <new name>");
            return;
        };
        let (old, new) = (old.trim(), new.trim());
        if old.is_empty() || new.is_empty() {
            self.set_err("usage: :subrename <old name> = <new name>");
            return;
        }
        self.send_sub_update(old, Some(new), None).await;
    }

    /// `:ignore <payee query>` — mark a detected series as NOT recurring; it drops
    /// out of the list (and reports/calendar). Un-ignore from the CLI:
    /// `arca recurring-confirm --match-key <k> --label ignore --dismiss`.
    async fn cmd_ignore(&mut self, query: &str) {
        match self.resolve_payee(query) {
            Ok((key, _)) => self.send_confirm(&key, Some("ignore"), None, true).await,
            Err(e) => self.set_err(e),
        }
    }

    /// Find the unique loaded series whose payee/name/descriptor contains `query`
    /// (case-insensitive). Returns its `(match_key, current_label)`. Ambiguous or
    /// empty matches are honest errors rather than a silent pick.
    fn resolve_payee(&self, query: &str) -> Result<(String, Option<String>), String> {
        let Some(r) = &self.data.recurring else {
            return Err("open the [c] recurring view first".into());
        };
        resolve_payee_in(&r.series, query)
    }

    /// Issue a `recurring.confirm` write and refresh the panel so the label
    /// column updates live. `display_name` renames the series (None leaves it).
    async fn send_confirm(
        &mut self,
        match_key: &str,
        label: Option<&str>,
        display_name: Option<&str>,
        active: bool,
    ) {
        let req = Request::RecurringConfirm {
            match_key: match_key.to_string(),
            label: label.map(str::to_string),
            display_name: display_name.map(str::to_string),
            business_tag: None,
            active: Some(active),
        };
        match call(self.conn.transport(), &req).await {
            Ok(Response::Ack) => {
                // Reload first — refresh_current() clears the status line — then
                // set the confirmation so it survives the reload.
                self.refresh_current().await;
                let note = if let Some(name) = display_name {
                    format!("renamed {match_key} → {name}")
                } else if !active {
                    format!("dismissed {match_key}")
                } else if label == Some("ignore") {
                    format!("ignored {match_key} (hidden as not recurring)")
                } else {
                    format!("labeled {match_key} = {}", label.unwrap_or("?"))
                };
                self.set_ok(note);
            }
            Ok(Response::Error(e)) => self.set_err(format!("err: {}: {}", e.code, e.msg)),
            Ok(other) => self.set_err(format!("unexpected: {other:?}")),
            Err(e) => self.set_err(format!("rpc: {e}")),
        }
    }

    /// The unconfirmed, filter-matching triage rows in display order — the set the
    /// Triage zone cursor indexes and `draw_recurring_table` renders. Shared so the
    /// cursor and the table never disagree about which payee is row N.
    fn bills_triage(&self) -> Vec<&arca_core::rpc::LabeledSeries> {
        let Some(r) = &self.data.recurring else {
            return Vec::new();
        };
        r.series
            .iter()
            .filter(|ls| ls.label.is_none())
            .filter(|ls| {
                let name = ls.display_name.as_deref().unwrap_or(&ls.series.display);
                hay_match(
                    &self.filter,
                    &format!("{} {}", name, cadence_label(ls.series.cadence)),
                )
            })
            .collect()
    }

    /// The confirmed detected series that graduated into the scheduled pane (label
    /// set, next charge still ahead), soonest-due first — the set the Schedule zone
    /// cursor indexes, so the operator can re-open the menu on a bill after labeling
    /// it. Declared subs + debt service in that pane aren't detected series and
    /// aren't selectable here. `r.series` is already sorted by `predicted_next`.
    fn bills_schedule(&self) -> Vec<&arca_core::rpc::LabeledSeries> {
        let Some(r) = &self.data.recurring else {
            return Vec::new();
        };
        let now = now_secs();
        r.series
            .iter()
            .filter(|ls| ls.label.is_some() && ls.series.predicted_next >= now)
            .collect()
    }

    /// The series the operator has hidden with Ignore — the Ignored zone's set, so
    /// each can be restored. Sent by the daemon in `RecurringPage.ignored` only
    /// when Bills requested `include_ignored`; empty otherwise.
    fn bills_ignored(&self) -> Vec<&arca_core::rpc::LabeledSeries> {
        match &self.data.recurring {
            Some(r) => r.ignored.iter().collect(),
            None => Vec::new(),
        }
    }

    /// The selectable *series* for the Triage/Ignored zones (the Schedule zone is
    /// heterogeneous — use [`bills_sched_rows`] there instead).
    fn bills_active(&self) -> Vec<&arca_core::rpc::LabeledSeries> {
        match self.bills_zone {
            BillsZone::Triage => self.bills_triage(),
            BillsZone::Ignored => self.bills_ignored(),
            // Not used for selection in the Schedule zone, but keep it total.
            BillsZone::Schedule => self.bills_schedule(),
        }
    }

    /// Every row of the scheduled pane (debt service + declared subs + confirmed
    /// detected series), sorted by due date — the set the Schedule-zone cursor
    /// walks, so j/k reaches *everything shown*, not just the editable detected
    /// ones. Shared with the renderer so the cursor and the pane stay in lockstep.
    fn bills_sched_rows(&self) -> Vec<SchedRow> {
        let mut rows: Vec<SchedRow> = Vec::new();
        if let Some(d) = &self.data.debt {
            for s in &d.scheduled {
                rows.push(SchedRow {
                    due: s.due_at,
                    desc: s.description.clone(),
                    amount: s.amount.to_string(),
                    tag: String::new(),
                    forecast: false,
                    kind: SchedKind::Debt,
                });
            }
            for s in &d.fixed {
                rows.push(SchedRow {
                    due: s.due_at,
                    desc: s.description.clone(),
                    amount: format!("~{}", s.amount),
                    tag: "sub".into(),
                    forecast: true,
                    kind: SchedKind::Declared {
                        name: s.description.clone(),
                    },
                });
            }
        }
        if let Some(r) = &self.data.recurring {
            let now = now_secs();
            for ls in &r.series {
                let Some(label) = ls.label.as_deref() else {
                    continue; // unconfirmed → triage, not the schedule
                };
                let p = ls.series.predicted_next;
                if p < now {
                    continue;
                }
                let name = ls
                    .display_name
                    .clone()
                    .unwrap_or_else(|| ls.series.display.clone());
                let est = Cents(ls.series.avg_amount.as_i64().abs());
                rows.push(SchedRow {
                    due: p,
                    desc: name,
                    amount: format!("~{est}"),
                    tag: label.to_string(),
                    forecast: true,
                    kind: SchedKind::Detected {
                        key: ls.series.payee.clone(),
                        label: label.to_string(),
                    },
                });
            }
        }
        rows.sort_by_key(|r| r.due);
        rows
    }

    /// Rows in a zone, for the Tab cycle and the cursor count. The Schedule zone
    /// counts *all* scheduled rows now, not just the detected ones.
    fn bills_zone_len(&self, zone: BillsZone) -> usize {
        match zone {
            BillsZone::Triage => self.bills_triage().len(),
            BillsZone::Schedule => self.bills_sched_rows().len(),
            BillsZone::Ignored => self.bills_ignored().len(),
        }
    }

    /// `(match_key, current_label, display_name)` of the cursor-selected series in
    /// the active zone — owned, so the caller can then mutate `self`. `None` when
    /// the active zone is empty.
    fn bills_selected(&self) -> Option<(String, Option<String>, String)> {
        let active = self.bills_active();
        if active.is_empty() {
            return None;
        }
        let ls = active[self.bills_sel.min(active.len() - 1)];
        let display = ls
            .display_name
            .clone()
            .unwrap_or_else(|| ls.series.display.clone());
        Some((ls.series.payee.clone(), ls.label.clone(), display))
    }

    /// Bills cursor + menu keys. Returns `true` when it consumed the key (so
    /// `handle_normal` skips the generic scroll/jump handlers). Tab flips the zone
    /// (only into a non-empty one); j/k move the cursor; Enter opens the action
    /// menu; 1/2/3 quick-label sub/bill/debt; x ignores. Anything else falls through
    /// (h/Esc back, `/` filter, `:`/`r`/letter jumps).
    async fn handle_bills_keys(&mut self, code: KeyCode) -> Result<bool> {
        if code == KeyCode::Tab {
            // Cycle triage -> scheduled -> ignored -> triage, landing on the next
            // zone that actually has rows (skip empties). If none other does, stay.
            const ORDER: [BillsZone; 3] =
                [BillsZone::Triage, BillsZone::Schedule, BillsZone::Ignored];
            let cur = ORDER
                .iter()
                .position(|z| *z == self.bills_zone)
                .unwrap_or(0);
            let next = (1..=3)
                .map(|step| ORDER[(cur + step) % 3])
                .find(|z| self.bills_zone_len(*z) > 0);
            match next {
                Some(z) if z != self.bills_zone => {
                    self.bills_zone = z;
                    self.bills_sel = 0;
                    self.scroll = 0;
                }
                _ => self.set_err("no other zone to select"),
            }
            return Ok(true);
        }
        let n = self.bills_zone_len(self.bills_zone);
        if n == 0 {
            return Ok(false);
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.bills_sel = (self.bills_sel + 1).min(n - 1);
                if self.bills_zone != BillsZone::Schedule {
                    self.follow_bills_cursor();
                }
                Ok(true)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.bills_sel = self.bills_sel.saturating_sub(1);
                if self.bills_zone != BillsZone::Schedule {
                    self.follow_bills_cursor();
                }
                Ok(true)
            }
            KeyCode::Enter => {
                self.open_bills_menu();
                Ok(true)
            }
            KeyCode::Char('1') => {
                self.quick_label_selected("sub").await;
                Ok(true)
            }
            KeyCode::Char('2') => {
                self.quick_label_selected("bill").await;
                Ok(true)
            }
            KeyCode::Char('3') => {
                self.quick_label_selected("debt").await;
                Ok(true)
            }
            KeyCode::Char('x') => {
                self.quick_label_selected("ignore").await;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Scroll the triage window so the cursor row stays visible. Reuses
    /// `self.scroll` as the window's first-visible row and the table's
    /// last-rendered body height (`self.viewport`) as the page size. Triage zone
    /// only — the scheduled pane is a fixed band that doesn't scroll.
    fn follow_bills_cursor(&mut self) {
        let vp = self.viewport.max(1);
        if self.bills_sel < self.scroll {
            self.scroll = self.bills_sel;
        } else if self.bills_sel >= self.scroll + vp {
            self.scroll = self.bills_sel + 1 - vp;
        }
    }

    /// Quick-label the cursor-selected series without opening the menu (1/2/3/x).
    /// In the Schedule zone this only acts on a detected series; declared subs and
    /// debt service have no quick-label (use Enter for their own menu). Re-clamps
    /// the cursor since a (re)labeled or ignored row moves between zones.
    async fn quick_label_selected(&mut self, label: &str) {
        let key = if self.bills_zone == BillsZone::Schedule {
            match self.bills_sched_selected_row().map(|r| r.kind) {
                Some(SchedKind::Detected { key, .. }) => key,
                _ => return, // declared/debt: no quick-label
            }
        } else {
            match self.bills_selected() {
                Some((key, _, _)) => key,
                None => return,
            }
        };
        self.send_confirm(&key, Some(label), None, true).await;
        let n = self.bills_zone_len(self.bills_zone);
        self.bills_sel = self.bills_sel.min(n.saturating_sub(1));
        if self.bills_zone != BillsZone::Schedule {
            self.follow_bills_cursor();
        }
    }

    /// The cursor-selected scheduled row (owned), or `None` if the schedule is
    /// empty. Only meaningful in the Schedule zone.
    fn bills_sched_selected_row(&self) -> Option<SchedRow> {
        let rows = self.bills_sched_rows();
        rows.get(self.bills_sel.min(rows.len().saturating_sub(1)))
            .cloned()
    }

    /// Open the action menu on the cursor-selected row. In the Schedule zone the
    /// menu depends on the row kind: a detected series gets the recurring menu, a
    /// declared sub gets Rename/Remove, debt service is informational. In
    /// Triage/Ignored it's always a detected series. No-op if the zone is empty.
    fn open_bills_menu(&mut self) {
        if self.bills_zone == BillsZone::Schedule {
            let Some(row) = self.bills_sched_selected_row() else {
                return;
            };
            let desc = row.desc;
            match row.kind {
                SchedKind::Detected { key, label } => {
                    self.build_recurring_menu(key, Some(label), desc);
                }
                SchedKind::Declared { name } => {
                    self.bills_menu = Some(BillsMenu {
                        match_key: name.clone(),
                        label: None,
                        title: name,
                        items: vec![
                            (BillsAction::SubRename, "Rename..."),
                            (BillsAction::SubRemove, "Remove (deactivate)"),
                        ],
                        sel: 0,
                    });
                }
                SchedKind::Debt => {
                    self.set_err("scheduled debt payment - not editable here");
                }
            }
            return;
        }
        let Some((key, label, display)) = self.bills_selected() else {
            return;
        };
        self.build_recurring_menu(key, label, display);
    }

    /// Build the recurring-series action menu (Subscription/Bill/Debt/Rename, plus
    /// Unlabel/Restore and Ignore as fitting) and open it. Shared by the
    /// Triage/Ignored selection and the Schedule zone's detected rows.
    fn build_recurring_menu(&mut self, key: String, label: Option<String>, display: String) {
        let ignored = label.as_deref() == Some("ignore");
        let mut items: Vec<(BillsAction, &'static str)> = vec![
            (BillsAction::Label("sub"), "Subscription"),
            (BillsAction::Label("bill"), "Bill"),
            (BillsAction::Label("debt"), "Debt"),
            (BillsAction::Rename, "Rename..."),
        ];
        if ignored {
            // Restore = dismiss the ignore label (the Unlabel action) → the series
            // returns to the triage inbox, unlabeled. No "Ignore" (already hidden).
            items.push((BillsAction::Unlabel, "Restore (un-ignore)"));
        } else {
            if label.is_some() {
                items.push((BillsAction::Unlabel, "Unlabel"));
            }
            items.push((BillsAction::Ignore, "Ignore (hide)"));
        }
        // Pre-select: Restore for an ignored series; else the current label, else the
        // inbuilt suggestion, else the top.
        let sel = if ignored {
            items
                .iter()
                .position(|(a, _)| matches!(a, BillsAction::Unlabel))
                .unwrap_or(0)
        } else {
            let pre = label.as_deref().or_else(|| {
                arca_core::recurring::suggest_label(&key)
                    .map(arca_core::recurring::SeriesLabel::as_str)
            });
            pre.and_then(|l| {
                items
                    .iter()
                    .position(|(a, _)| matches!(a, BillsAction::Label(x) if *x == l))
            })
            .unwrap_or(0)
        };
        self.bills_menu = Some(BillsMenu {
            match_key: key,
            label,
            title: display,
            items,
            sel,
        });
    }

    /// Drive the open action menu: j/k move, Enter executes, Esc/q closes. Borrows
    /// are scoped per arm so executing/closing doesn't alias the menu state.
    async fn handle_bills_menu(&mut self, code: KeyCode) {
        let n = match &self.bills_menu {
            Some(m) => m.items.len(),
            None => return,
        };
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.bills_menu = None,
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(m) = &mut self.bills_menu {
                    m.sel = (m.sel + 1).min(n - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(m) = &mut self.bills_menu {
                    m.sel = m.sel.saturating_sub(1);
                }
            }
            KeyCode::Enter => self.exec_bills_menu().await,
            _ => {}
        }
    }

    /// Execute the highlighted menu action and close the menu. Label/Ignore confirm
    /// via `recurring.confirm`; Unlabel soft-dismisses the current label; Rename
    /// drops into Cmd mode pre-filled with `rename <payee> = ` (the one place typing
    /// is unavoidable — the name is invented, not a fixed keyword).
    async fn exec_bills_menu(&mut self) {
        let Some(menu) = self.bills_menu.take() else {
            return;
        };
        let (action, _) = &menu.items[menu.sel.min(menu.items.len() - 1)];
        match action {
            BillsAction::Label(l) => {
                self.send_confirm(&menu.match_key, Some(l), None, true)
                    .await;
            }
            BillsAction::Ignore => {
                self.send_confirm(&menu.match_key, Some("ignore"), None, true)
                    .await;
            }
            BillsAction::Unlabel => match &menu.label {
                Some(lbl) => {
                    self.send_confirm(&menu.match_key, Some(lbl), None, false)
                        .await;
                }
                None => self.set_err("series is not labeled"),
            },
            BillsAction::Rename => {
                self.mode = Mode::Cmd;
                self.buf = format!("rename {} = ", menu.match_key);
                return; // entering input; the clamp/refresh below doesn't apply
            }
            // Declared-subscription actions (Schedule zone) go through the separate
            // manual.update_subscription verb, keyed by the sub's name.
            BillsAction::SubRename => {
                self.mode = Mode::Cmd;
                self.buf = format!("subrename {} = ", menu.match_key);
                return;
            }
            BillsAction::SubRemove => {
                self.send_sub_update(&menu.match_key, None, Some(false))
                    .await;
            }
        }
        let n = self.bills_zone_len(self.bills_zone);
        self.bills_sel = self.bills_sel.min(n.saturating_sub(1));
        if self.bills_zone != BillsZone::Schedule {
            self.follow_bills_cursor();
        }
    }

    /// Issue a `manual.update_subscription` (rename and/or deactivate a declared
    /// sub) and refresh so the schedule reflects it.
    async fn send_sub_update(&mut self, name: &str, new_name: Option<&str>, active: Option<bool>) {
        let req = Request::ManualUpdateSubscription {
            name: name.to_string(),
            new_name: new_name.map(str::to_string),
            active,
        };
        match call(self.conn.transport(), &req).await {
            Ok(Response::Ack) => {
                self.refresh_current().await;
                let note = if let Some(nn) = new_name {
                    format!("renamed {name} → {nn}")
                } else if active == Some(false) {
                    format!("removed {name} from the schedule")
                } else {
                    format!("updated {name}")
                };
                let n = self.bills_zone_len(self.bills_zone);
                self.bills_sel = self.bills_sel.min(n.saturating_sub(1));
                self.set_ok(note);
            }
            Ok(Response::Error(e)) => self.set_err(format!("err: {}: {}", e.code, e.msg)),
            Ok(other) => self.set_err(format!("unexpected: {other:?}")),
            Err(e) => self.set_err(format!("rpc: {e}")),
        }
    }
}

pub fn render<B: Backend>(term: &mut Terminal<B>, app: &mut App) -> Result<()> {
    // The draw closure borrows `app` immutably; collect the rendered content
    // dimensions out through these locals and store them after the draw so the
    // key handler can clamp scrolling against the live view size.
    let mut content_len = 0;
    let mut viewport = 0;
    term.draw(|f| {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Min(0),    // body
                Constraint::Length(1), // status
            ])
            .split(area);

        draw_summary(f, chunks[0], app);
        let (cl, vp) = draw_body(f, chunks[1], app);
        content_len = cl;
        viewport = vp;
        draw_status(f, chunks[2], app);
    })?;
    app.content_len = content_len;
    app.viewport = viewport;
    Ok(())
}

/// Case-insensitive fuzzy match: every non-space char of `filter` must appear in
/// `hay` in order (subsequence). Empty filter matches everything. Whitespace in
/// the filter is ignored, so a multi-word query still matches across the joined
/// fields a view feeds in. Looser than substring on purpose — `aamem` finds
/// "Aqua membership".
fn hay_match(filter: &str, hay: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let hay = hay.to_lowercase();
    let mut hc = hay.chars();
    'needle: for fc in filter.to_lowercase().chars() {
        if fc.is_whitespace() {
            continue;
        }
        for hch in hc.by_ref() {
            if hch == fc {
                continue 'needle;
            }
        }
        return false; // ran out of haystack before matching this char
    }
    true
}

/// Resolve a `:label`/`:unlabel` payee query against the loaded series by
/// case-insensitive substring (payee key, raw descriptor, or display name).
/// Unique hit → `(match_key, current_label)`; zero or many → an error message.
fn resolve_payee_in(
    series: &[arca_core::rpc::LabeledSeries],
    query: &str,
) -> Result<(String, Option<String>), String> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Err("empty payee query".into());
    }
    let hits: Vec<&arca_core::rpc::LabeledSeries> = series
        .iter()
        .filter(|ls| {
            ls.series.payee.contains(&q)
                || ls.series.display.to_lowercase().contains(&q)
                || ls
                    .display_name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(&q))
        })
        .collect();
    match hits.as_slice() {
        [] => Err(format!("no series matches {q:?}")),
        [one] => Ok((one.series.payee.clone(), one.label.clone())),
        many => Err(format!(
            "ambiguous ({} match): {}",
            many.len(),
            many.iter()
                .map(|ls| ls.series.payee.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Longest common prefix of a set of strings (byte-wise; our verb list is ASCII).
/// Used by TAB completion when several verbs share a prefix.
fn common_prefix(items: &[&str]) -> String {
    let Some((first, rest)) = items.split_first() else {
        return String::new();
    };
    let mut end = first.len();
    for s in rest {
        end = end.min(s.len());
        while !first.is_char_boundary(end) || first[..end] != s[..end] {
            end -= 1;
        }
    }
    first[..end].to_string()
}

/// Render a vertical scrollbar on the right border of `area`. No-op when all
/// content already fits (`total <= viewport`).
fn render_scrollbar(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    total: usize,
    pos: usize,
    viewport: usize,
) {
    if total <= viewport {
        return;
    }
    let mut state = ScrollbarState::new(total).position(pos);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None);
    f.render_stateful_widget(
        bar,
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut state,
    );
}

/// Draw a scrollable paragraph (a flat list of lines) inside `block`, with a
/// scrollbar. Returns `(total_lines, viewport_height)` for scroll clamping.
fn draw_lines(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    block: Block<'_>,
    lines: Vec<Line<'_>>,
) -> (usize, usize) {
    let total = lines.len();
    let viewport = block.inner(area).height as usize;
    let scroll = app.scroll.min(total.saturating_sub(viewport));
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((scroll as u16, 0)),
        area,
    );
    render_scrollbar(f, area, total, scroll, viewport);
    (total, viewport)
}

/// Draw a scrollable table inside `block`, windowing rows by the scroll offset
/// (the header row stays pinned). Returns `(total_rows, viewport_rows)`.
fn draw_table(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    block: Block<'_>,
    header: Row<'_>,
    rows: Vec<Row<'_>>,
    widths: &[Constraint],
) -> (usize, usize) {
    let total = rows.len();
    if total == 0 {
        // Distinguish a genuinely empty view from a filter that matched nothing,
        // so the reader isn't staring at a blank table wondering which it is.
        let msg = if app.filter.is_empty() {
            "- nothing here -".to_string()
        } else {
            format!("- no rows match \"{}\" - Esc clears the filter", app.filter)
        };
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return (0, 0);
    }
    // One body row is consumed by the header.
    let viewport = (block.inner(area).height as usize).saturating_sub(1);
    let start = app.scroll.min(total.saturating_sub(viewport));
    let visible: Vec<Row<'_>> = rows.into_iter().skip(start).collect();
    let table = Table::new(visible, widths.to_vec())
        .header(header)
        .block(block);
    f.render_widget(table, area);
    render_scrollbar(f, area, total, start, viewport);
    (total, viewport)
}

/// Short, all-caps name of a view — used at the right of the summary bar.
fn view_title(view: View) -> &'static str {
    match view {
        View::Menu => "MENU",
        View::Money => "ACCOUNTS",
        View::Business => "BUSINESS",
        View::Pp => "INVEST",
        View::Bills => "BILLS",
        View::Tx => "TRANSACTIONS",
        View::Alerts => "ALERTS",
        View::Charts => "CHARTS",
        View::Expenses => "EXPENSES",
        View::Help => "HELP",
    }
}

/// The always-on summary bar (top line of every frame): the money headline that
/// never leaves the frame — net worth (+ 30-day delta), open debt, month-to-date
/// cash flow, the pending-alert count — and the current view name at the right.
/// Reads whatever is cached in `app.data` (the menu seeds every field on launch
/// and persists it across view switches); a missing leg degrades to `—` rather
/// than blanking the bar.
fn draw_summary(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);

    // Net worth + 30-day delta.
    spans.push(Span::styled("NW ", dim));
    match &app.data.money {
        Some(m) => spans.push(Span::styled(
            money_short(m.net_worth.as_i64() as f64 / 100.0),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        None => spans.push(Span::raw("-")),
    }
    if let Some(d) = app.data.charts.as_ref().and_then(networth_delta_30d) {
        let color = if d < 0 { Color::Red } else { Color::Green };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            money_short_signed(d),
            Style::default().fg(color),
        ));
    }

    spans.push(Span::styled(" · Debt ", dim));
    match &app.data.debt {
        Some(d) => spans.push(Span::raw(money_short(d.total_open.as_i64() as f64 / 100.0))),
        None => spans.push(Span::raw("-")),
    }

    spans.push(Span::styled(" · MTD ", dim));
    match app.data.charts.as_ref().and_then(|c| c.cash_flow.last()) {
        Some(m) => {
            let net = m.income.as_i64() + m.expenses.as_i64();
            let color = if net < 0 { Color::Red } else { Color::Green };
            spans.push(Span::styled(
                money_short_signed(net),
                Style::default().fg(color),
            ));
        }
        None => spans.push(Span::raw("-")),
    }

    spans.push(Span::styled(" · ", dim));
    let pending = pending_alert_count(app);
    if pending > 0 {
        spans.push(Span::styled(
            format!("!{pending}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    } else if app.data.alerts.is_some() {
        spans.push(Span::styled("ok", Style::default().fg(Color::Green)));
    } else {
        spans.push(Span::raw("-"));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);

    // Current view name, right-aligned on the same row (clipped first on a narrow
    // terminal, since it's the least important thing on the bar).
    let right = Line::from(vec![
        Span::styled(
            "arca",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", view_title(app.view)), dim),
    ]);
    f.render_widget(Paragraph::new(right).right_aligned(), area);
}

/// Net-worth change over the last ~30 days: latest snapshot minus the earliest
/// snapshot at or after the 30-day cutoff (or the oldest we have, if history is
/// shorter). `None` if there are no points.
fn networth_delta_30d(c: &ChartsSnapshot) -> Option<i64> {
    let last = c.net_worth.last()?;
    let cutoff = last.at_secs - 30 * 86_400;
    let base = c.net_worth.iter().find(|p| p.at_secs >= cutoff)?;
    Some(last.amount.as_i64() - base.amount.as_i64())
}

/// Count of pending (undelivered) alerts — the `!N` on the summary bar. 0 if the
/// alerts snapshot hasn't loaded.
fn pending_alert_count(app: &App) -> usize {
    app.data
        .alerts
        .as_ref()
        .map(|a| a.rows.iter().filter(|r| !r.delivered).count())
        .unwrap_or(0)
}

/// Context-aware footer hint: short, specific to where the operator is. On the
/// Main menu it shows the cursor/open keys; in a view it shows scroll + back +
/// that view's own verbs. The full key list lives in `?` help.
fn status_hint(app: &App) -> String {
    // The action menu, when open, owns the keys.
    if app.bills_menu.is_some() {
        return "j/k move · Enter choose · Esc cancel".to_string();
    }
    if app.view == View::Menu {
        return "j/k move · Enter open · 1-7/letter jump · r reload · ? help · q quit".to_string();
    }
    // Bills owns a cursor, so its hint is the confirm flow, not "j/k scroll". Tab
    // cycles the zones (triage inbox / confirmed bills / ignored).
    if app.view == View::Bills {
        return if app.bills_zone == BillsZone::Ignored {
            "j/k pick · Enter menu (Restore) · Tab next zone · h back".to_string()
        } else {
            "j/k pick · Enter menu · 1/2/3 label · x ignore · Tab zone · h back".to_string()
        };
    }
    let verbs = match app.view {
        View::Charts => " · [ ] window",
        View::Business => " · :set tag=<biz>",
        View::Tx | View::Alerts | View::Expenses => " · / filter",
        _ => "",
    };
    format!("j/k scroll · h/Esc back{verbs} · r reload · ? help · q quit")
}

fn draw_status(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let line = match app.mode {
        Mode::Normal if app.status.is_empty() => Line::from(Span::raw(status_hint(app))),
        // A message overrides the keybind hint; red on failure, green on success.
        Mode::Normal => {
            let color = if app.status_is_err {
                Color::Red
            } else {
                Color::Green
            };
            Line::from(Span::styled(app.status.clone(), Style::default().fg(color)))
        }
        Mode::Cmd => Line::from(vec![Span::raw(":"), Span::raw(app.buf.clone())]),
        Mode::Search => Line::from(vec![Span::raw("/"), Span::raw(app.buf.clone())]),
    };
    f.render_widget(
        Paragraph::new(line).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn draw_body(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    match app.view {
        View::Menu => draw_menu(f, area, app),
        View::Money => draw_money(f, area, app),
        View::Business => draw_business(f, area, app),
        View::Pp => draw_pp(f, area, app),
        View::Bills => draw_bills(f, area, app),
        View::Tx => draw_tx(f, area, app),
        View::Alerts => draw_alerts(f, area, app),
        View::Charts => draw_charts(f, area, app),
        View::Expenses => draw_expenses(f, area, app),
        View::Help => draw_help(f, area, app),
    }
}

/// Current unix seconds. Used by the dashboard's 30-day "upcoming" window.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Compact signed dollar label from cents: `+$4.2k`, `-$1.0k`, `+$430`. Used on
/// the summary bar (Δ30d, MTD) and the menu teasers (cash-flow avg, business net).
fn money_short_signed(cents: i64) -> String {
    let s = money_short(cents.abs() as f64 / 100.0);
    if cents < 0 {
        format!("-{s}")
    } else {
        format!("+{s}")
    }
}

/// The home launcher (Main menu) — fills the terminal. On a wide screen the
/// destination tiles sit in a left panel; the right side stacks a full Recent-
/// transactions table over an Upcoming-30d panel. Narrow screens stack all three.
/// The summary bar is drawn globally above. The cursor (`menu_sel`) highlights a
/// tile — `j`/`k` move it, `Enter` opens it — and the tile letter jumps from
/// anywhere. Returns `(0, 0)` (nothing scrolls).
fn draw_menu(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    let up = upcoming_30d(app);
    let up_h = (up.len() as u16 + 2).clamp(3, 12);
    if area.width >= 100 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);
        draw_menu_tiles(f, cols[0], app);
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(up_h)])
            .split(cols[1]);
        draw_menu_recent(f, right[0], app);
        draw_menu_upcoming(f, right[1], &up);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(MENU_ITEMS.len() as u16 + 4),
                Constraint::Min(4),
                Constraint::Length(up_h.min(8)),
            ])
            .split(area);
        draw_menu_tiles(f, rows[0], app);
        draw_menu_recent(f, rows[1], app);
        draw_menu_upcoming(f, rows[2], &up);
    }
    (0, 0)
}

/// The launcher panel: one row per destination tile (cursor-highlighted) with its
/// live teaser, then a key-hint footer.
fn draw_menu_tiles(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" menu ");
    let dim = Style::default().fg(Color::DarkGray);
    let sel_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    for (i, (key, name, view)) in MENU_ITEMS.iter().enumerate() {
        let sel = i == app.menu_sel;
        let marker = if sel { ">" } else { " " };
        let key_style = if sel {
            sel_style
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let name_style = if sel { sel_style } else { Style::default() };
        lines.push(Line::from(vec![
            Span::raw(format!("  {marker} ")),
            Span::styled((*key).to_string(), key_style),
            Span::styled(format!("  {name:<13}"), name_style),
            Span::styled(menu_teaser(app, *view), dim),
        ]));
    }
    // Fill the lower panel with glanceable detail (the keybinds that used to sit
    // here are redundant with the global footer). Sections degrade away when their
    // data is absent. Header helper:
    let header = |lines: &mut Vec<Line<'static>>, title: &str| {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {title}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    };

    if let Some(m) = &app.data.money {
        header(&mut lines, "net worth by kind");
        for k in &m.by_kind {
            lines.push(Line::from(format!(
                "    {:<12} {:>14}",
                k.kind,
                money_short(k.total.as_i64() as f64 / 100.0)
            )));
        }
    }

    // This month's cash flow (in / out / net), from the latest monthly bucket.
    if let Some(mf) = app.data.charts.as_ref().and_then(|c| c.cash_flow.last()) {
        let net = mf.income.as_i64() + mf.expenses.as_i64();
        header(&mut lines, "this month");
        lines.push(Line::from(vec![
            Span::raw("    in "),
            Span::styled(
                money_short(mf.income.as_i64() as f64 / 100.0),
                Style::default().fg(Color::Green),
            ),
            Span::raw("  out "),
            Span::styled(
                money_short(mf.expenses.as_i64().unsigned_abs() as f64 / 100.0),
                Style::default().fg(Color::Red),
            ),
            Span::raw("  net "),
            Span::styled(
                money_short_signed(net),
                Style::default().fg(if net < 0 { Color::Red } else { Color::Green }),
            ),
        ]));
    }

    // API & subscription spend lines (usage-based providers), if any are wired.
    if let Some(m) = &app.data.money {
        if !m.subscriptions.is_empty() {
            header(&mut lines, "API & subs");
            for s in &m.subscriptions {
                let v = match s.currency.as_str() {
                    "USD" => s.latest.to_string(),
                    "CREDITS" => format!("{} cr", s.latest.as_i64()),
                    "MESSAGES" => format!("{} msg", s.latest.as_i64()),
                    other => format!("{} {other}", s.latest.as_i64()),
                };
                lines.push(Line::from(format!(
                    "    {:<24} {:>10}",
                    truncate(&s.name, 24),
                    v
                )));
            }
        }
    }

    // Pending alerts (the summaries, not just the !N count) when any are queued.
    if let Some(a) = &app.data.alerts {
        let pending: Vec<&arca_core::rpc::AlertRow> =
            a.rows.iter().filter(|r| !r.delivered).collect();
        if !pending.is_empty() {
            header(&mut lines, "alerts pending");
            for r in pending.iter().take(4) {
                lines.push(Line::from(Span::styled(
                    format!("    ! {}", truncate(&r.summary, 34)),
                    Style::default().fg(Color::Yellow),
                )));
            }
        }
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The Recent-activity panel: a full transaction table that fills its pane height
/// (the widget clips to whatever rows fit). Bumped fetch (15) feeds it.
fn draw_menu_recent(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" recent ");
    let Some(t) = app.data.tx.as_ref().filter(|t| !t.rows.is_empty()) else {
        let msg = if app.data.tx.is_some() {
            "- no transactions -"
        } else {
            "loading..."
        };
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };
    let header = Row::new(vec!["posted", "account", "amount", "tag", "description"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row<'_>> = t
        .rows
        .iter()
        .map(|r| {
            Row::new(vec![
                display_short_date(r.posted_at),
                truncate(&r.account, 18),
                r.amount.to_string(),
                r.tag.clone().unwrap_or_default(),
                r.description.clone().unwrap_or_default(),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(11),
        Constraint::Length(18),
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Min(16),
    ];
    f.render_widget(Table::new(rows, widths).header(header).block(block), area);
}

/// The Upcoming panel: scheduled debt service + confirmed recurring within 30d.
fn draw_menu_upcoming(f: &mut ratatui::Frame<'_>, area: Rect, up: &[(i64, String, String)]) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" upcoming · 30d ");
    if up.is_empty() {
        f.render_widget(
            Paragraph::new("- nothing due in 30 days -")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    }
    let lines: Vec<Line<'_>> = up
        .iter()
        .map(|(date, what, amt)| {
            Line::from(format!(
                "  {:<11} {:<26} {:>14}",
                display_short_date(*date),
                truncate(what, 26),
                amt
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Everything due in the next 30 days — scheduled debt service + confirmed
/// recurring series' predicted charge — soonest first. Recurring amount is the
/// observed average (an estimate, flagged `~…est`).
fn upcoming_30d(app: &App) -> Vec<(i64, String, String)> {
    let now = now_secs();
    let horizon = now + 30 * 86_400;
    let mut out: Vec<(i64, String, String)> = Vec::new();
    if let Some(d) = &app.data.debt {
        // Scheduled debt service + declared recurring obligations (rent etc.) —
        // both are money going out on a date, so both ride the 30-day window.
        for s in d.scheduled.iter().chain(d.fixed.iter()) {
            if s.due_at >= now && s.due_at <= horizon {
                out.push((s.due_at, s.description.clone(), s.amount.to_string()));
            }
        }
    }
    if let Some(r) = &app.data.recurring {
        for ls in &r.series {
            if ls.label.is_none() {
                continue; // confirmed only
            }
            let p = ls.series.predicted_next;
            if p >= now && p <= horizon {
                let name = ls
                    .display_name
                    .clone()
                    .unwrap_or_else(|| ls.series.display.clone());
                // avg is stored as an outflow (negative); show the magnitude.
                let est = Cents(ls.series.avg_amount.as_i64().abs());
                out.push((p, name, format!("~{est} est")));
            }
        }
    }
    out.sort_by_key(|(d, _, _)| *d);
    out
}

/// One-line live teaser for a menu tile, built from whatever snapshot is cached.
/// Degrades to `…` (not yet loaded) so a slow leg never blanks the row.
fn menu_teaser(app: &App, view: View) -> String {
    match view {
        View::Money => match &app.data.money {
            Some(m) => {
                let accts: u32 = m.by_kind.iter().map(|k| k.account_count).sum();
                format!(
                    "{accts} accts · {}",
                    money_short(m.net_worth.as_i64() as f64 / 100.0)
                )
            }
            None => "-".into(),
        },
        View::Tx => match app.data.tx.as_ref().and_then(|t| t.rows.first()) {
            Some(r) => format!(
                "last: {} {}",
                truncate(r.description.as_deref().unwrap_or("-"), 12),
                money_short_signed(r.amount.as_i64())
            ),
            None => "-".into(),
        },
        View::Bills => {
            if app.data.recurring.is_none() && app.data.debt.is_none() {
                return "-".into(); // not loaded — don't imply "0 debts"
            }
            let rec = app.data.recurring.as_ref().map_or(0, |r| r.series.len());
            let (debts, total) = app.data.debt.as_ref().map_or((0, Cents(0)), |d| {
                (
                    d.open_balances
                        .iter()
                        .filter(|b| b.balance.as_i64() != 0)
                        .count(),
                    d.total_open,
                )
            });
            // No `next:` here — the Upcoming panel already shows it, and this
            // teaser must fit the narrow launcher column.
            format!(
                "{rec} recurring · {debts} debts {}",
                money_short(total.as_i64() as f64 / 100.0)
            )
        }
        View::Pp => match &app.data.pp {
            Some(p) => {
                let breach = p.rows.iter().filter(|r| r.band_breach).count();
                let state = if breach > 0 {
                    format!("{breach} out of band")
                } else {
                    "in band".into()
                };
                format!(
                    "T2 {} · {state}",
                    money_short(p.total.as_i64() as f64 / 100.0)
                )
            }
            None => "-".into(),
        },
        View::Charts => match &app.data.charts {
            Some(c) if !c.cash_flow.is_empty() => {
                let n = c.cash_flow.len() as i64;
                let net: i64 = c
                    .cash_flow
                    .iter()
                    .map(|m| m.income.as_i64() + m.expenses.as_i64())
                    .sum();
                let arrow = networth_delta_30d(c).map_or("·", |d| if d < 0 { "v" } else { "^" });
                format!("NW {arrow} · cash flow {}/mo", money_short_signed(net / n))
            }
            _ => "-".into(),
        },
        View::Alerts => {
            let p = pending_alert_count(app);
            if p > 0 {
                format!("{p} pending !")
            } else if app.data.alerts.is_some() {
                "none pending".into()
            } else {
                "-".into()
            }
        }
        View::Business => match &app.data.business {
            Some(b) => format!("{} · net {}", b.tag, money_short_signed(b.net.as_i64())),
            None => "-".into(),
        },
        _ => String::new(),
    }
}

/// The Bills deep view: a full-width status band (debt rollup · recurring label
/// breakdown · the charges due in the next 30d) over a debt-detail band (open
/// card balances beside the schedule) over the unconfirmed-detection triage
/// table (scrollable; the `:label`/`:ignore`/`:rename` Cmd verbs operate on it).
/// Composes the debt + recurring snapshots `refresh_bills` fetches. Returns the
/// triage table's scroll metrics. Confirmed recurring series graduate into the
/// schedule band above; only unconfirmed detections remain in the table — kept
/// full width so its columns don't clip in a side pane.
fn draw_bills(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    let dim = Style::default().fg(Color::DarkGray);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("bills (debt · recurring subs/bills/debts)");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let vsplit = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(inner);

    // --- status band: debt + recurring rollup, then the next charges due ---
    let (mut subs, mut bills_n, mut debts_n, mut unlabeled) = (0u32, 0u32, 0u32, 0u32);
    if let Some(r) = &app.data.recurring {
        for ls in &r.series {
            match ls.label.as_deref() {
                Some("sub") => subs += 1,
                Some("bill") => bills_n += 1,
                Some("debt") => debts_n += 1,
                _ => unlabeled += 1,
            }
        }
    }
    let mut line1: Vec<Span<'static>> = vec![Span::styled("Debt ", dim)];
    match &app.data.debt {
        Some(d) => {
            let cards = d
                .open_balances
                .iter()
                .filter(|b| b.balance.as_i64() != 0)
                .count();
            line1.push(Span::styled(d.total_open.to_string(), bold));
            line1.push(Span::styled(format!(" · {cards} cards"), dim));
        }
        None => line1.push(Span::raw("-")),
    }
    line1.push(Span::styled("     Recurring: ", dim));
    if app.data.recurring.is_some() {
        line1.push(Span::raw(format!(
            "{subs} sub · {bills_n} bill · {debts_n} debt"
        )));
        if unlabeled > 0 {
            line1.push(Span::styled(format!(" · {unlabeled} unlabeled"), dim));
        }
    } else {
        line1.push(Span::raw("-"));
    }

    let up = upcoming_30d(app);
    let now = now_secs();
    let mut line2: Vec<Span<'static>> = vec![Span::styled(
        "→ next 30d: ",
        Style::default().fg(Color::Cyan),
    )];
    if up.is_empty() {
        line2.push(Span::styled("nothing due", dim));
    } else {
        for (i, (due, name, amt)) in up.iter().take(3).enumerate() {
            if i > 0 {
                line2.push(Span::styled(" · ", dim));
            }
            let days = ((*due - now) / 86_400).max(0);
            let when = if days == 0 {
                "today".to_string()
            } else {
                format!("in {days}d")
            };
            line2.push(Span::raw(format!("{} {amt} ({when})", truncate(name, 16))));
        }
    }
    f.render_widget(
        Paragraph::new(vec![Line::from(line1), Line::from(line2)]),
        vsplit[0],
    );

    // --- debt-detail band: open balances beside scheduled debt service ---
    let body = vsplit[1];
    // Confirmed detected series with a future predicted charge graduate into the
    // scheduled pane (parity with the menu's upcoming panel); unconfirmed ones
    // stay in the triage table below. Count them so the band sizes correctly.
    let confirmed_recurring_n = app.data.recurring.as_ref().map_or(0, |r| {
        let now = now_secs();
        r.series
            .iter()
            .filter(|ls| ls.label.is_some() && ls.series.predicted_next >= now)
            .count()
    });
    let (open_n, sched_n) = match &app.data.debt {
        Some(d) => (
            d.open_balances
                .iter()
                .filter(|b| b.balance.as_i64() != 0)
                .count(),
            d.scheduled.len() + d.fixed.len() + confirmed_recurring_n,
        ),
        None => (1, 1),
    };
    // Band tall enough for the longer of the two lists (+3: borders + a spare
    // row), capped so it never crowds out the recurring table below.
    let band_h = (open_n.max(sched_n) as u16 + 3).clamp(5, 10);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(band_h), Constraint::Min(4)])
        .split(body);

    // open balances (most owed first)
    let mut bal_lines: Vec<Line<'static>> = Vec::new();
    match &app.data.debt {
        Some(d) => {
            let mut bals: Vec<_> = d
                .open_balances
                .iter()
                .filter(|b| b.balance.as_i64() != 0)
                .collect();
            bals.sort_by_key(|b| std::cmp::Reverse(b.balance.as_i64()));
            if bals.is_empty() {
                bal_lines.push(Line::from(Span::styled("none open", dim)));
            }
            for o in &bals {
                bal_lines.push(Line::from(format!(
                    "{:<22} {:>14}",
                    truncate(&o.account_name, 22),
                    o.balance.to_string()
                )));
            }
        }
        None => bal_lines.push(Line::from(Span::styled("loading...", dim))),
    }
    let bal_pane = Paragraph::new(bal_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" open balances "),
    );

    // The scheduled pane lists everything due — debt service + declared subs +
    // confirmed detected series — sorted by date, via the same `bills_sched_rows`
    // the Schedule-zone cursor walks. The cursor walks ALL of them (so j/k reaches
    // declared subs like Rent too), and the selected row is simply the one at
    // `bills_sel`; highlight by index. Forecasts render cyan + ~; exact payments
    // plain. None in the other zones.
    let sched_rows = app.bills_sched_rows();
    let sched_sel = if app.bills_zone == BillsZone::Schedule && !sched_rows.is_empty() {
        Some(app.bills_sel.min(sched_rows.len() - 1))
    } else {
        None
    };
    let mut sch_lines: Vec<Line<'static>> = Vec::new();
    if app.data.debt.is_none() {
        sch_lines.push(Line::from(Span::styled("loading...", dim)));
    } else if sched_rows.is_empty() {
        sch_lines.push(Line::from(Span::styled("none scheduled", dim)));
    } else {
        for (i, row) in sched_rows.iter().enumerate() {
            let selected = sched_sel == Some(i);
            let style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if row.forecast {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            let marker = if selected { ">" } else { " " };
            let suffix = if row.tag.is_empty() {
                String::new()
            } else {
                format!("  {}", row.tag)
            };
            sch_lines.push(Line::from(Span::styled(
                format!(
                    "{} {:<7} {:<14} {:>11}{}",
                    marker,
                    display_short_date(row.due),
                    truncate(&row.desc, 14),
                    row.amount,
                    suffix
                ),
                style,
            )));
        }
    }
    let sch_title = if app.bills_zone == BillsZone::Schedule {
        " scheduled · recurring  (Tab: triage · Enter menu) "
    } else {
        " scheduled · recurring "
    };
    let sch_pane =
        Paragraph::new(sch_lines).block(Block::default().borders(Borders::ALL).title(sch_title));

    // Wide: balances beside scheduled. Narrow: balances only (the status line's
    // "next 30d" already surfaces the soonest scheduled charge).
    if rows[0].width >= 100 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[0]);
        f.render_widget(bal_pane, cols[0]);
        f.render_widget(sch_pane, cols[1]);
    } else {
        f.render_widget(bal_pane, rows[0]);
    }

    let metrics = draw_recurring_table(f, rows[1], app);
    // The action menu floats over the whole Bills view when open.
    if let Some(menu) = &app.bills_menu {
        draw_bills_menu(f, area, menu);
    }
    metrics
}

/// Render the Bills action popup centered over the view: a bordered list with the
/// selected item marked. `Clear` wipes the cells beneath so the panes don't bleed
/// through. The footer hint (j/k · Enter · Esc) rides the status line.
fn draw_bills_menu(f: &mut ratatui::Frame<'_>, area: Rect, menu: &BillsMenu) {
    let w = 34u16;
    let h = menu.items.len() as u16 + 2; // borders
    let rect = centered_rect(w, h, area);
    f.render_widget(ratatui::widgets::Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" {} ", truncate(&menu.title, 26)));
    let lines: Vec<Line<'_>> = menu
        .items
        .iter()
        .enumerate()
        .map(|(i, (_, text))| {
            let on = i == menu.sel;
            let marker = if on { "> " } else { "  " };
            let style = if on {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!("{marker}{text}"), style))
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// A centered `Rect` of at most `w`×`h`, clamped to `area`.
fn centered_rect(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw_money(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    let block = Block::default().borders(Borders::ALL).title("net worth");
    let Some(m) = &app.data.money else {
        f.render_widget(
            Paragraph::new("loading... press r to refresh").block(block),
            area,
        );
        return (0, 0);
    };
    let mut text = vec![
        Line::from(vec![
            Span::styled("Net worth: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(m.net_worth.to_string()),
        ]),
        Line::from(""),
    ];
    // Each kind = a header (aggregate total + count), then one row per account
    // beneath it (largest first, as sorted daemon-side), so the operator sees
    // every individual asset, not just the rollup. Non-USD accounts show their
    // native currency and a `*` (excluded from net worth, no FX in v1).
    for k in &m.by_kind {
        text.push(Line::from(Span::styled(
            format!(
                "{:<12} {:>16}   ({} accounts)",
                k.kind,
                k.total.to_string(),
                k.account_count
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for acct in m.accounts.iter().filter(|a| a.kind == k.kind) {
            let bal = if acct.currency == "USD" {
                acct.balance.to_string()
            } else {
                format!("{} {} *", acct.balance.as_i64(), acct.currency)
            };
            text.push(Line::from(format!("    {:<28} {:>16}", acct.name, bal)));
        }
    }
    // A non-USD account whose kind never made it into by_kind (every account of
    // that kind was excluded) still gets listed under its own header.
    let kinds_shown: std::collections::HashSet<&str> =
        m.by_kind.iter().map(|k| k.kind.as_str()).collect();
    let mut orphan_kinds: Vec<&str> = m
        .accounts
        .iter()
        .filter(|a| !kinds_shown.contains(a.kind.as_str()))
        .map(|a| a.kind.as_str())
        .collect();
    orphan_kinds.sort_unstable();
    orphan_kinds.dedup();
    for kind in orphan_kinds {
        text.push(Line::from(Span::styled(
            format!("{kind:<12}   (excluded from net worth)"),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for acct in m.accounts.iter().filter(|a| a.kind == kind) {
            text.push(Line::from(format!(
                "    {:<28} {:>16}",
                acct.name,
                format!("{} {} *", acct.balance.as_i64(), acct.currency)
            )));
        }
    }
    if !m.subscriptions.is_empty() {
        text.push(Line::from(""));
        text.push(Line::from(Span::styled(
            "API & subscriptions",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for s in &m.subscriptions {
            let formatted = match s.currency.as_str() {
                "USD" => s.latest.to_string(),
                "CREDITS" => format!("{} credits", s.latest.as_i64()),
                "MESSAGES" => format!("{} msgs", s.latest.as_i64()),
                other => format!("{} {other}", s.latest.as_i64()),
            };
            text.push(Line::from(format!("  {:<32} {:>20}", s.name, formatted)));
        }
    }
    text.push(Line::from(""));
    text.push(Line::from(Span::styled(
        format!("as of {}", display_ast(m.asof_secs)),
        Style::default().fg(Color::DarkGray),
    )));
    draw_lines(f, area, app, block, text)
}

fn draw_business(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("business - {}", app.business_tag));
    let Some(b) = &app.data.business else {
        f.render_widget(
            Paragraph::new("loading... press r to refresh").block(block),
            area,
        );
        return (0, 0);
    };
    let lines = vec![
        Line::from(format!("Tag:    {} ({})", b.tag, b.display_name)),
        Line::from(format!("Scope:  {:?}", b.scope)),
        Line::from(""),
        Line::from(format!("Income:   {:>16}", b.income.to_string())),
        Line::from(format!("Expenses: {:>16}", b.expenses.to_string())),
        Line::from(Span::styled(
            format!("Net:      {:>16}", b.net.to_string()),
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    draw_lines(f, area, app, block, lines)
}

/// Dollar target for a targeted sleeve: `target_pct` of the T2 total. Zero for
/// untargeted rows (`gold_etf` / `other`, whose `target_pct` is 0).
fn sleeve_target_cents(r: &arca_core::pp::DriftRow, total: i64) -> i64 {
    (r.target_pct / 100.0 * total as f64).round() as i64
}

/// Drift in *dollars* for a targeted sleeve: actual − target. Positive =
/// overweight (sell to rebalance), negative = underweight (buy). The number the
/// operator acts on, vs `drift_pp` which is only the relative gap.
fn sleeve_drift_cents(r: &arca_core::pp::DriftRow, total: i64) -> i64 {
    r.actual_cents.as_i64() - sleeve_target_cents(r, total)
}

fn draw_pp(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    let title = "permanent portfolio (T2 33×3 target · rebalance bands 22/44)";
    let block = Block::default().borders(Borders::ALL).title(title);
    let Some(p) = &app.data.pp else {
        f.render_widget(
            Paragraph::new("loading... press r to refresh").block(block),
            area,
        );
        return (0, 0);
    };
    let total = p.total.as_i64();

    let inner = block.inner(area);
    f.render_widget(block, area);
    let vsplit = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(inner);
    let status_area = vsplit[0];
    let body = vsplit[1];

    // --- status line + actionable rebalance call ---
    // On a breach, Rowland rebalances the *whole* T2 back to the 33.3% target (not
    // just to the band edge): so the move per sleeve is its dollar drift — sell the
    // overweights, buy the underweights. The moves net to ~zero.
    let mut status_lines: Vec<Line<'static>> = Vec::new();
    if p.band_breach {
        status_lines.push(Line::from(vec![
            Span::styled(
                "BAND BREACH",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("   T2 total {}", p.total), Style::default()),
        ]));
        let mut moves: Vec<Span<'static>> = Vec::new();
        for r in &p.rows {
            if r.upper_band_pct <= 0.0 {
                continue; // untargeted sleeve — no rebalance target
            }
            let d = sleeve_drift_cents(r, total);
            if d.abs() < 10_000 {
                continue; // < $100 — rounding noise, not a trade
            }
            let (verb, color) = if d > 0 {
                ("sell", Color::Red)
            } else {
                ("buy", Color::Green)
            };
            if !moves.is_empty() {
                moves.push(Span::raw(" · "));
            }
            moves.push(Span::styled(
                format!(
                    "{verb} {} {}",
                    sleeve_abbr(&r.asset_class),
                    money_short(d.abs() as f64 / 100.0)
                ),
                Style::default().fg(color),
            ));
        }
        let mut reb = vec![Span::styled(
            "→ rebalance: ",
            Style::default().fg(Color::Cyan),
        )];
        if moves.is_empty() {
            reb.push(Span::styled(
                "within rounding",
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            reb.extend(moves);
        }
        status_lines.push(Line::from(reb));
    } else {
        status_lines.push(Line::from(vec![
            Span::styled("T2 in band", Style::default().fg(Color::Green)),
            Span::styled(format!("   T2 total {}", p.total), Style::default()),
        ]));
        status_lines.push(Line::from(Span::styled(
            "→ no rebalance needed",
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(status_lines), status_area);

    // --- allocation bars: actual % per sleeve, green in-band / red breached ---
    let bars: Vec<Bar<'_>> = p
        .rows
        .iter()
        .map(|r| {
            let color = if r.band_breach {
                Color::Red
            } else if r.upper_band_pct > 0.0 {
                Color::Green
            } else {
                Color::DarkGray // untargeted sleeve (gold_etf / other)
            };
            Bar::default()
                .value(r.actual_pct.round() as u64)
                .label(Line::from(sleeve_abbr(&r.asset_class)))
                .text_value(format!("{:.0}%", r.actual_pct))
                .style(Style::default().fg(color))
        })
        .collect();
    let drift_bar = BarChart::default()
        .block(Block::default().borders(Borders::ALL).title(" allocation "))
        .data(BarGroup::default().bars(&bars))
        .bar_width(7)
        .bar_gap(2)
        .max(50);

    // --- sleeve drift table: actual $/%, target $, drift in pp AND dollars ---
    let header = Row::new(vec![
        "sleeve", "actual", "%", "target $", "dpp", "d$", "status",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row<'_>> = p
        .rows
        .iter()
        .map(|r| {
            let targeted = r.upper_band_pct > 0.0;
            let (target_s, delta_s, delta_style) = if targeted {
                let d = sleeve_drift_cents(r, total);
                let style = if r.band_breach {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default()
                };
                (
                    Cents(sleeve_target_cents(r, total)).to_string(),
                    money_short_signed(d),
                    style,
                )
            } else {
                ("-".into(), "-".into(), Style::default().fg(Color::DarkGray))
            };
            let status = if r.band_breach {
                Span::styled("BREACH", Style::default().fg(Color::Red))
            } else if targeted {
                Span::styled("ok", Style::default().fg(Color::Green))
            } else {
                Span::styled("-", Style::default().fg(Color::DarkGray))
            };
            Row::new(vec![
                Span::raw(sleeve_abbr(&r.asset_class).to_string()).into(),
                Span::raw(r.actual_cents.to_string()).into(),
                Span::raw(format!("{:>5.1}", r.actual_pct)).into(),
                Span::raw(target_s).into(),
                Span::raw(format!("{:>+5.1}", r.drift_pp)).into(),
                Span::styled(delta_s, delta_style).into(),
                ratatui::text::Text::from(status),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(8),
        Constraint::Length(14),
        Constraint::Length(6),
        Constraint::Length(14),
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Length(7),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" sleeves "));

    // --- T1 backbone: hold-forever, info only ---
    let bb = &p.backbone;
    let short = |c: Cents| money_short(c.as_i64() as f64 / 100.0);
    let bb_lines = vec![
        Line::from(format!(
            "  gold {} · silver {} · xmr {}",
            short(bb.gold),
            short(bb.silver),
            short(bb.xmr),
        )),
        Line::from(format!("  land {} · sfr {}", short(bb.land), short(bb.sfr))),
        Line::from(Span::styled(
            format!("  total {}", bb.total),
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    let backbone = Paragraph::new(bb_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" T1 backbone (hold-forever) ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    // --- lay out the body: wide → table left, bars over backbone right; narrow →
    // stacked. The table gets the most room either way (it carries the data). ---
    if body.width >= 100 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(56), Constraint::Percentage(44)])
            .split(body);
        f.render_widget(table, cols[0]);
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(5)])
            .split(cols[1]);
        f.render_widget(drift_bar, right[0]);
        f.render_widget(backbone, right[1]);
    } else {
        let rows3 = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(9),
                Constraint::Min(5),
                Constraint::Length(5),
            ])
            .split(body);
        f.render_widget(drift_bar, rows3[0]);
        f.render_widget(table, rows3[1]);
        f.render_widget(backbone, rows3[2]);
    }
    (0, 0) // fixed multi-pane layout; nothing to scroll
}

/// Short sleeve label for the narrow drift bars.
fn sleeve_abbr(asset_class: &str) -> &str {
    match asset_class {
        "equity" => "equity",
        "long_treasuries" => "bonds",
        "cash" => "cash",
        "gold_etf" => "gold",
        other => other,
    }
}

fn draw_tx(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    let block = Block::default().borders(Borders::ALL).title("transactions");
    let Some(t) = &app.data.tx else {
        f.render_widget(
            Paragraph::new("loading... press r to refresh").block(block),
            area,
        );
        return (0, 0);
    };
    let header = Row::new(vec!["posted", "account", "amount", "tag", "description"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row<'_>> = t
        .rows
        .iter()
        .filter(|r| {
            hay_match(
                &app.filter,
                &format!(
                    "{} {} {}",
                    r.account,
                    r.tag.as_deref().unwrap_or(""),
                    r.description.as_deref().unwrap_or("")
                ),
            )
        })
        .map(|r| {
            Row::new(vec![
                display_date(r.posted_at),
                r.account.clone(),
                r.amount.to_string(),
                r.tag.clone().unwrap_or_default(),
                r.description.clone().unwrap_or_default(),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(18),
        Constraint::Length(20),
        Constraint::Length(16),
        Constraint::Length(12),
        Constraint::Min(20),
    ];
    draw_table(f, area, app, block, header, rows, &widths)
}

fn draw_alerts(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    let Some(a) = &app.data.alerts else {
        let block = Block::default().borders(Borders::ALL).title("alerts");
        f.render_widget(
            Paragraph::new("loading... press r to refresh").block(block),
            area,
        );
        return (0, 0);
    };
    // Two panes: what's armed (fixed) over what's fired (scrollable). Showing the
    // armed rules makes it obvious alerts ARE configured even when nothing fired.
    let rules_h = (a.rules.len() as u16 + 2).clamp(3, 8);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(rules_h), Constraint::Min(3)])
        .split(area);
    draw_armed_rules(f, chunks[0], &a.rules);
    draw_recent_alerts(f, chunks[1], app, &a.rows)
}

fn draw_armed_rules(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    rules: &[arca_core::rpc::AlertRuleRow],
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("armed rules ({})", rules.len()));
    if rules.is_empty() {
        f.render_widget(
            Paragraph::new("none armed - set one with `arca alert-set` / alert.upsert.")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    }
    let lines: Vec<Line<'_>> = rules
        .iter()
        .map(|r| {
            Line::from(format!(
                "• {}  [{}] → {}",
                r.name,
                r.kind.as_deref().unwrap_or("?"),
                r.channel
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_recent_alerts(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    rows: &[arca_core::rpc::AlertRow],
) -> (usize, usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("recent alerts");
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new("none fired yet.")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return (0, 0);
    }
    let header = Row::new(vec!["fired", "", "rule", "summary"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let trows: Vec<Row<'_>> = rows
        .iter()
        .filter(|r| hay_match(&app.filter, &format!("{} {}", r.rule_name, r.summary)))
        .map(|r| {
            // Undelivered = still in the push queue; flag it.
            let (mark, style) = if r.delivered {
                ("ok", Style::default().fg(Color::DarkGray))
            } else {
                ("●", Style::default().fg(Color::Yellow))
            };
            Row::new(vec![
                display_ast(r.fired_at),
                mark.to_string(),
                r.rule_name.clone(),
                r.summary.clone(),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(22),
        Constraint::Length(2),
        Constraint::Length(16),
        Constraint::Min(20),
    ];
    draw_table(f, area, app, block, header, trows, &widths)
}

fn cadence_label(c: arca_core::recurring::Cadence) -> &'static str {
    use arca_core::recurring::Cadence;
    match c {
        Cadence::Weekly => "weekly",
        Cadence::Biweekly => "biweekly",
        Cadence::Monthly => "monthly",
        Cadence::Quarterly => "quarterly",
        Cadence::Yearly => "yearly",
    }
}

/// The triage table (scrollable body of the Bills deep view): one row per
/// UNconfirmed detected payee, with a row cursor and a one-key confirm flow.
/// Confirmed series have graduated into the scheduled pane above, so only the
/// triage inbox lives here. Columns: cadence, next due, payee, observed last/avg,
/// count, and the inbuilt label `suggest`ion (the known-merchant guess; `-` when
/// arca has none — the operator's call). The `bills_sel` cursor highlights a row;
/// `app.scroll` is its window anchor (kept in range by `follow_bills_cursor`).
/// Filterable via `/`. The `:label`/`:ignore`/`:rename` Cmd verbs still work as a
/// power-user path. Returns `(total_rows, viewport_rows)` so the cursor's window
/// math has the live body height.
fn draw_recurring_table(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    // This pane is the cursor surface for both the Triage zone (unconfirmed inbox)
    // and the Ignored zone (hidden series, to restore). The Schedule zone selects in
    // the scheduled pane above instead — when it's active this still shows the triage
    // list, just without the cursor.
    let ignored_view = app.bills_zone == BillsZone::Ignored;
    let title = if ignored_view {
        "ignored · hidden  (Tab: back · Enter: Restore / re-label · j/k pick)"
    } else {
        "detected · unconfirmed  (Tab: confirmed bills · j/k pick · Enter menu · 1/2/3 · x)"
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let Some(r) = &app.data.recurring else {
        f.render_widget(
            Paragraph::new("loading... press r to refresh").block(block),
            area,
        );
        return (0, 0);
    };
    // Triage zone (and Schedule, which shows it un-cursored) lists the unconfirmed
    // inbox; the Ignored zone lists the hidden series. Same cursor index either way.
    let rows_src = if ignored_view {
        app.bills_ignored()
    } else {
        app.bills_triage()
    };
    if rows_src.is_empty() {
        let msg = if ignored_view {
            "nothing ignored.".to_string()
        } else if r.series.is_empty() {
            "no recurring payees yet - needs >=3 charges at a regular cadence.".to_string()
        } else if app.filter.is_empty() {
            "nothing to triage - every detected series is labeled (see the schedule above)."
                .to_string()
        } else {
            format!(
                "- no detections match \"{}\" - Esc clears the filter",
                app.filter
            )
        };
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return (0, 0);
    }

    let total = rows_src.len();
    let sel = app.bills_sel.min(total - 1);
    // The cursor shows here in the Triage and Ignored zones; in the Schedule zone it
    // highlights a row in the scheduled pane above instead.
    let cursor_here = app.bills_zone != BillsZone::Schedule;
    let viewport = (block.inner(area).height as usize).saturating_sub(1); // header row
    let start = app.scroll.min(total.saturating_sub(viewport));

    let header = Row::new(vec![
        "", "cadence", "next due", "payee", "last", "avg", "n", "suggest",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    // "last" is the most recent actual charge; "avg" the running average — the next
    // amount is unknown until it posts, so we never claim one.
    let rows: Vec<Row<'_>> = rows_src
        .iter()
        .enumerate()
        .map(|(i, ls)| {
            let s = &ls.series;
            let payee = ls.display_name.as_deref().unwrap_or(&s.display);
            // The inbuilt guess — the headline of this feature. Green when arca
            // recognizes the merchant; dim "-" when it's the operator's call.
            let suggest = match arca_core::recurring::suggest_label(&s.payee) {
                Some(lbl) => Cell::from(lbl.as_str()).style(Style::default().fg(Color::Green)),
                None => Cell::from("-").style(Style::default().fg(Color::DarkGray)),
            };
            let on_cursor = i == sel && cursor_here;
            let marker = if on_cursor { ">" } else { " " };
            let row = Row::new(vec![
                Cell::from(marker),
                Cell::from(cadence_label(s.cadence)),
                Cell::from(display_short_date(s.predicted_next)),
                Cell::from(truncate(payee, 22)),
                Cell::from(s.last_amount.to_string()),
                Cell::from(s.avg_amount.to_string()),
                Cell::from(s.count.to_string()),
                suggest,
            ]);
            if on_cursor {
                row.style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                row
            }
        })
        .skip(start)
        .take(viewport.max(1))
        .collect();
    let widths = [
        Constraint::Length(1),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Min(14),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(3),
        Constraint::Length(8),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
    render_scrollbar(f, area, total, start, viewport);
    (total, viewport)
}

// ---- charts view ----

/// How many months of cash-flow are shown at once in the Charts view; h/l slide
/// this window over the wider fetched history (see `App::charts_scroll`).
const CHART_WINDOW_MONTHS: usize = 12;

/// Days since the Unix epoch for a proleptic-Gregorian civil date (Hinnant's
/// algorithm). Avoids pulling `chrono` into the TUI just to turn a `YYYY-MM`
/// label into a timestamp for windowing the net-worth line.
fn days_from_civil(mut y: i64, m: i64, d: i64) -> i64 {
    if m <= 2 {
        y -= 1;
    }
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // Mar=0 … Feb=11
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Parse a `YYYY-MM` label to the unix-seconds start of that month (UTC).
fn month_start_unix(label: &str) -> Option<i64> {
    let (ys, ms) = label.split_once('-')?;
    let y: i64 = ys.trim().parse().ok()?;
    let m: i64 = ms.trim().parse().ok()?;
    if !(1..=12).contains(&m) {
        return None;
    }
    Some(days_from_civil(y, m, 1) * 86_400)
}

/// Unix-seconds start of the month *after* a `YYYY-MM` label (window upper bound).
fn next_month_start_unix(label: &str) -> Option<i64> {
    let (ys, ms) = label.split_once('-')?;
    let y: i64 = ys.trim().parse().ok()?;
    let m: i64 = ms.trim().parse().ok()?;
    if !(1..=12).contains(&m) {
        return None;
    }
    let (ny, nm) = if m >= 12 { (y + 1, 1) } else { (y, m + 1) };
    Some(days_from_civil(ny, nm, 1) * 86_400)
}

fn draw_charts(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    let title = Block::default().borders(Borders::ALL).title("charts");
    let Some(c) = &app.data.charts else {
        f.render_widget(
            Paragraph::new("loading... press r to refresh").block(title),
            area,
        );
        return (0, 0);
    };
    // Window the cash-flow months: show CHART_WINDOW_MONTHS ending at the newest,
    // shifted back by `charts_offset` (h/l). The net-worth line is filtered to the
    // same calendar span so both panes scroll together through history.
    let n = c.cash_flow.len();
    let win = CHART_WINDOW_MONTHS.min(n.max(1));
    let max_off = n.saturating_sub(win);
    let off = app.charts_offset.min(max_off);
    let start = max_off - off;
    let cf_win: &[MonthFlow] = if n == 0 {
        &c.cash_flow
    } else {
        &c.cash_flow[start..start + win]
    };
    let nw_win: Vec<TimePoint> = match (cf_win.first(), cf_win.last()) {
        (Some(first), Some(last)) => {
            let t0 = month_start_unix(&first.label).unwrap_or(i64::MIN);
            let t1 = next_month_start_unix(&last.label).unwrap_or(i64::MAX);
            c.net_worth
                .iter()
                .filter(|p| p.at_secs >= t0 && p.at_secs < t1)
                .cloned()
                .collect()
        }
        _ => c.net_worth.clone(),
    };

    // Three series, space-filling: wide → net-worth trend full-width on top,
    // cash-flow bars beside top-spend categories below; narrow → all three
    // stacked. Category spend (folded in from the old Expenses view) is always
    // "this month", independent of the h/l history window.
    if area.width >= 100 {
        let v = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
            .split(area);
        draw_networth_line(f, v[0], &nw_win);
        let bottom = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(v[1]);
        draw_cashflow_bars(f, bottom[0], cf_win);
        draw_charts_spend(f, bottom[1], app);
    } else {
        let v = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(40),
                Constraint::Percentage(35),
                Constraint::Percentage(25),
            ])
            .split(area);
        draw_networth_line(f, v[0], &nw_win);
        draw_cashflow_bars(f, v[1], cf_win);
        draw_charts_spend(f, v[2], app);
    }
    // Footer hint when there's more history to scroll into.
    if max_off > 0 {
        let pos = format!(
            " h/l < scroll {}-{} of {} months > ",
            start + 1,
            start + win,
            n
        );
        let hint = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(pos).right_aligned())
                .style(Style::default().fg(Color::DarkGray)),
            hint,
        );
    }
    (0, 0) // fixed panes; vertical scroll unused (h/l drives horizontal window)
}

/// The Charts view's spend pane: top expense categories (this month) as the same
/// horizontal bars the Expenses view uses. Reads the categories snapshot Charts
/// now co-fetches; placeholders until it loads / when nothing was spent.
fn draw_charts_spend(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    match &app.data.categories {
        Some(c) if !c.rows.is_empty() => draw_spending_bars(f, area, &c.rows),
        other => {
            let msg = if other.is_some() {
                "no expenses this month."
            } else {
                "loading... press r"
            };
            f.render_widget(
                Paragraph::new(msg)
                    .style(Style::default().fg(Color::DarkGray))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("top categories"),
                    ),
                area,
            );
        }
    }
}

fn draw_networth_line(f: &mut ratatui::Frame<'_>, area: Rect, points: &[TimePoint]) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("net worth over time");
    if points.len() < 2 {
        f.render_widget(
            Paragraph::new("need >=2 balance snapshots for a trend line.").block(block),
            area,
        );
        return;
    }
    // x = sample index (snapshots are roughly daily; even spacing reads fine),
    // y = dollars.
    let data: Vec<(f64, f64)> = points
        .iter()
        .enumerate()
        .map(|(i, p)| (i as f64, p.amount.as_i64() as f64 / 100.0))
        .collect();
    let n = data.len();
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for (_, y) in &data {
        lo = lo.min(*y);
        hi = hi.max(*y);
    }
    // Y-axis tick labels: round values, count scaled to pane height (taller
    // pane → more gridline labels). ratatui spaces the labels evenly, so evenly
    // spaced round values line up with their positions. nice_yticks floors/ceils
    // the bounds, so the line isn't glued to the top/bottom border.
    let inner_h = area.height.saturating_sub(3); // top+bottom border + x-axis row
    let yticks = nice_yticks(lo, hi, (inner_h / 3).clamp(2, 8) as usize);
    let (axis_lo, axis_hi) = (yticks[0], yticks[yticks.len() - 1]);

    let datasets = vec![
        // Braille: 2x4 sub-cell dots per glyph give the high-detail dotted line
        // (matches the budget_tracker_tui reference). Needs a terminal font with
        // braille coverage — if the line renders blank, that font lacks U+28xx.
        Dataset::default()
            .marker(Marker::Braille)
            // Line, not Scatter: scatter plots only the sampled points, so the
            // climbs between flat plateaus have no dots and the line vanishes on
            // the way up. Line interpolates, drawing the rise as braille dots.
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Cyan))
            .data(&data),
    ];
    let x_axis = Axis::default().bounds([0.0, (n - 1) as f64]).labels(vec![
        Span::raw(display_short_date(points[0].at_secs)),
        Span::raw(display_short_date(points[n - 1].at_secs)),
    ]);
    let y_axis = Axis::default().bounds([axis_lo, axis_hi]).labels(
        yticks
            .iter()
            .map(|&v| Span::raw(money_short(v)))
            .collect::<Vec<_>>(),
    );
    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(x_axis)
        .y_axis(y_axis);
    f.render_widget(chart, area);
}

fn draw_cashflow_bars(f: &mut ratatui::Frame<'_>, area: Rect, months: &[MonthFlow]) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("monthly cash flow  (green in / red out)");
    if months.is_empty() {
        f.render_widget(
            Paragraph::new("no transactions in the window.").block(block),
            area,
        );
        return;
    }
    // Reserve the bottom line for the window's dollar totals — the bars show
    // relative size; the operator wants the actual money in/out below them.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    // One group per month: an income bar and an expense-magnitude bar, each
    // labeled with its own dollar amount (text_value) so the operator reads the
    // money in/out for that month directly off the chart.
    let groups: Vec<BarGroup<'_>> = months
        .iter()
        .map(|m| {
            let income_usd = m.income.as_i64().max(0) as f64 / 100.0;
            let expense_usd = (-m.expenses.as_i64()).max(0) as f64 / 100.0;
            BarGroup::default()
                .label(Line::from(month_abbr(&m.label)))
                // $ tags go in each bar's LABEL row (its own row below the bar),
                // not the in-bar text_value: ratatui fills the whole bar_width in
                // the value row, so a short number gets flanked by colored fill
                // ("bar comes down next to the tag"). A bar label has no fill
                // behind it. text_value is blanked with a space so the raw value
                // doesn't print in-bar; the month rides the group label one row
                // below the $ tags.
                // text_value must be present (else ratatui prints the raw value
                // in-bar) but invisible: a space clears the fill and leaves a
                // black notch at the bar base, so give value_style the bar's
                // color as bg — the space cell then reads as solid bar fill.
                .bars(&[
                    Bar::default()
                        .value(income_usd as u64)
                        .text_value(" ".to_string())
                        .value_style(Style::default().fg(Color::Green).bg(Color::Green))
                        .label(Line::from(Span::styled(
                            money_short(income_usd),
                            Style::default().fg(Color::Green),
                        )))
                        .style(Style::default().fg(Color::Green)),
                    Bar::default()
                        .value(expense_usd as u64)
                        .text_value(" ".to_string())
                        .value_style(Style::default().fg(Color::Red).bg(Color::Red))
                        .label(Line::from(Span::styled(
                            money_short(expense_usd),
                            Style::default().fg(Color::Red),
                        )))
                        .style(Style::default().fg(Color::Red)),
                ])
        })
        .collect();
    let mut chart = BarChart::default()
        .block(block)
        .bar_width(6)
        .bar_gap(0)
        .group_gap(2);
    for g in groups {
        chart = chart.data(g);
    }
    f.render_widget(chart, chunks[0]);

    // Totals over the visible window: money gained (in), money spent (out), net.
    let total_in: i64 = months.iter().map(|m| m.income.as_i64()).sum();
    let total_out: i64 = months.iter().map(|m| m.expenses.as_i64().min(0)).sum();
    let net = total_in + total_out;
    let net_style = if net < 0 {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    let totals = Line::from(vec![
        Span::raw("  in "),
        Span::styled(
            Cents(total_in).to_string(),
            Style::default().fg(Color::Green),
        ),
        Span::raw("    out "),
        Span::styled(
            Cents(total_out.abs()).to_string(),
            Style::default().fg(Color::Red),
        ),
        Span::raw("    net "),
        Span::styled(Cents(net).to_string(), net_style),
    ]);
    f.render_widget(Paragraph::new(totals), chunks[1]);
}

// ---- expenses view ----

fn draw_expenses(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    let title = Block::default()
        .borders(Borders::ALL)
        .title("expenses by category (this month)");
    let Some(c) = &app.data.categories else {
        f.render_widget(
            Paragraph::new("loading... press r to refresh").block(title),
            area,
        );
        return (0, 0);
    };
    if c.rows.is_empty() {
        f.render_widget(
            Paragraph::new("no expenses recorded in this window.").block(title),
            area,
        );
        return (0, 0);
    }
    // Top: horizontal spending bar (top categories, unfiltered). Bottom: a
    // scrollable, filterable table of all categories — scroll applies there.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(12), Constraint::Min(4)])
        .split(area);
    draw_spending_bars(f, chunks[0], &c.rows);
    draw_expense_table(f, chunks[1], app, &c.rows, c.total)
}

fn draw_spending_bars(f: &mut ratatui::Frame<'_>, area: Rect, rows: &[CategorySpend]) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("top categories");
    let avail = (block.inner(area).height as usize).max(1);
    let bars: Vec<Bar<'_>> = rows
        .iter()
        .take(avail)
        .map(|r| {
            let mag = (-r.amount.as_i64()).max(0); // cents, positive
            Bar::default()
                .value((mag / 100) as u64)
                .label(Line::from(truncate(&r.category, 18)))
                .text_value(r.amount.to_string())
                .style(Style::default().fg(Color::Magenta))
        })
        .collect();
    let chart = BarChart::default()
        .block(block)
        .direction(Direction::Horizontal)
        .bar_width(1)
        .bar_gap(0)
        .data(BarGroup::default().bars(&bars));
    f.render_widget(chart, area);
}

fn draw_expense_table(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    rows: &[CategorySpend],
    total: Cents,
) -> (usize, usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("all categories - total {total}"));
    let header = Row::new(vec!["category", "amount", "share"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let total_mag = (-total.as_i64()).max(1) as f64;
    let trows: Vec<Row<'_>> = rows
        .iter()
        .filter(|r| hay_match(&app.filter, &r.category))
        .map(|r| {
            let share = (-r.amount.as_i64()) as f64 / total_mag * 100.0;
            Row::new(vec![
                r.category.clone(),
                r.amount.to_string(),
                format!("{share:>5.1}%"),
            ])
        })
        .collect();
    let widths = [
        Constraint::Min(20),
        Constraint::Length(16),
        Constraint::Length(8),
    ];
    draw_table(f, area, app, block, header, trows, &widths)
}

/// Compact dollar label for chart axes: `$17.0k`, `$1.2M`, `$430`.
fn money_short(dollars: f64) -> String {
    let a = dollars.abs();
    if a >= 1_000_000.0 {
        format!("${:.1}M", dollars / 1_000_000.0)
    } else if a >= 1_000.0 {
        format!("${:.1}k", dollars / 1_000.0)
    } else {
        format!("${dollars:.0}")
    }
}

/// Round Y-axis tick values spanning the data `[lo, hi]`, aiming for ~`target`
/// ticks. Picks a "nice" step (1/2/2.5/5 × 10ⁿ) and floors/ceils the bounds to
/// it, so labels read as round numbers ($5.0k, $10.0k …) and the outer ticks
/// give the line a little headroom. Ascending: `[0]` is the bottom label, the
/// last is the top. Always ≥2 ticks, even for flat data.
fn nice_yticks(lo: f64, hi: f64, target: usize) -> Vec<f64> {
    let target = target.max(2);
    let range = (hi - lo).max(1.0);
    let raw = range / (target - 1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let nice = match raw / mag {
        n if n <= 1.0 => 1.0,
        n if n <= 2.0 => 2.0,
        n if n <= 2.5 => 2.5,
        n if n <= 5.0 => 5.0,
        _ => 10.0,
    };
    let step = nice * mag;
    let start = (lo / step).floor() * step;
    let mut end = (hi / step).ceil() * step;
    if end <= start {
        end = start + step;
    }
    let mut ticks = Vec::new();
    let mut v = start;
    while v <= end + step * 0.5 && ticks.len() < 24 {
        // collapse -0.0 → 0.0 so the label reads "$0", not "$-0"
        ticks.push(if v == 0.0 { 0.0 } else { v });
        v += step;
    }
    if ticks.len() < 2 {
        ticks.push(start + step);
    }
    ticks
}

/// `"2026-05"` → `"May"`. Falls back to the raw label if it doesn't parse.
fn month_abbr(label: &str) -> String {
    const NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    match label
        .split_once('-')
        .and_then(|(_, m)| m.parse::<usize>().ok())
    {
        Some(m) if (1..=12).contains(&m) => NAMES[m - 1].to_string(),
        _ => label.to_string(),
    }
}

/// Truncate to `max` chars on a char boundary, appending `…` if cut.
/// Fold glyphs the operator's console font can't render down to ASCII. The
/// router terminal shows any missing glyph as a bare `_`, so em-dashes, arrows,
/// box-marks, ellipses and the like (all above Latin-1) become readable ASCII.
/// The middle dot `·` (U+00B7) renders fine and is deliberately kept. Applied to
/// externally-sourced text (account names carry an em-dash, e.g. `OpenRouter —
/// main`; descriptions come from Plaid) via `truncate`.
fn ascii_glyph(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '—' | '–' => out.push('-'),
            '▸' | '►' | '▶' | '→' => out.push('>'),
            '◀' | '←' => out.push('<'),
            '↑' | '▲' => out.push('^'),
            '↓' | '▼' => out.push('v'),
            '↻' => out.push('*'),
            '⚠' => out.push('!'),
            '✓' => out.push('+'),
            '✗' => out.push('x'),
            '…' => out.push_str("..."),
            '≥' => out.push_str(">="),
            '≤' => out.push_str("<="),
            'Δ' => out.push('d'),
            other => out.push(other),
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    let s = ascii_glyph(s);
    if s.chars().count() <= max {
        s
    } else if max <= 2 {
        s.chars().take(max).collect()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(2)).collect();
        out.push_str("..");
        out
    }
}

fn draw_help(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) -> (usize, usize) {
    let block = Block::default().borders(Borders::ALL).title("help");
    let body = [
        "Main menu (home)",
        "  j/k or arrow keys  move the cursor between tiles",
        "  Enter       open the selected tile",
        "  1-7         open a tile directly (the tile order)",
        "  q           quit (only from the menu)",
        "",
        "Views",
        "  h / Esc   back to the menu  (q also backs out of a view)",
        "  j / k     scroll (down/up)",
        "  Ctrl-d/u  half page · PgDn/PgUp full page · gg/G top/bottom",
        "  [ / ]     charts: slide the month window ({ } by a page)",
        "  /<text>   fuzzy-filter (tx, recurring, alerts, expenses); Esc clears",
        "",
        "Jump keys (open a view from anywhere)",
        "  o menu   m accounts   t txns   d/c bills   p invest",
        "  v charts   a alerts   b business   e expenses",
        "",
        "Bills (manage detected payees - no typing needed)",
        "  Tab       cycle cursor zones: triage inbox -> confirmed bills -> ignored",
        "  j / k     move the cursor",
        "  Enter     open the action menu (suggestion pre-picked; Enter again accepts)",
        "  1 / 2 / 3 quick-label sub / bill / debt   ·   x   ignore (hide)",
        "  menu: Subscription/Bill/Debt · Rename · Unlabel · Ignore (j/k, Enter, Esc)",
        "  schedule zone: cursor walks every row; Enter on a declared sub (Rent) ->",
        "                 Rename / Remove; debt-service rows are informational",
        "  ignored zone: Enter -> Restore (un-ignore) brings a hidden series back",
        "  (the : commands below still work as a power-user path)",
        "",
        "Commands (:)",
        "  :q                          quit",
        "  :refresh <kind>             provider refresh (e.g. :refresh plaid)",
        "  :set tag=<biz>              change the business tag",
        "  :label <payee> <sub|bill|debt>   label a recurring series (TAB completes)",
        "  :unlabel <payee>            drop a series label",
        "  :rename <payee> = <name>    rename any series",
        "  :ignore <payee>             hide a false-positive series",
        "",
        "  r refresh · ? this help · q quit",
    ];
    let text: Vec<Line<'_>> = body.iter().map(|s| Line::from(*s)).collect();
    draw_lines(f, area, app, block, text)
}

pub async fn event_loop<B: Backend>(term: &mut Terminal<B>, app: &mut App) -> Result<()> {
    app.refresh_current().await;
    while !app.quit {
        render(term, app)?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    app.handle_key(k.code, k.modifiers).await?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(Connection {
            socket: Some(PathBuf::from("/nonexistent.sock")),
            tcp: None,
        })
    }

    fn labeled(payee: &str, display: &str, label: Option<&str>) -> arca_core::rpc::LabeledSeries {
        use arca_core::recurring::{Cadence, Series};
        arca_core::rpc::LabeledSeries {
            series: Series {
                payee: payee.into(),
                display: display.into(),
                cadence: Cadence::Monthly,
                count: 3,
                first_seen: 0,
                last_seen: 0,
                last_amount: Cents(-100),
                min_amount: Cents(-100),
                max_amount: Cents(-100),
                avg_amount: Cents(-100),
                predicted_next: 0,
            },
            label: label.map(str::to_string),
            display_name: None,
            confirmed_at: None,
        }
    }

    #[test]
    fn resolve_payee_unique_ambiguous_and_missing() {
        let series = vec![
            labeled("spotify", "Spotify USA", Some("sub")),
            labeled("peacock", "Peacock TV", None),
            labeled("amazon prime video", "Amazon Prime Video", None),
        ];
        // Unique substring -> match, carries the current label through.
        assert_eq!(
            resolve_payee_in(&series, "spot").unwrap(),
            ("spotify".to_string(), Some("sub".to_string()))
        );
        // Case-insensitive, matches the raw descriptor too.
        assert_eq!(
            resolve_payee_in(&series, "PEACOCK").unwrap().0,
            "peacock".to_string()
        );
        // Unlabeled series -> None current label.
        assert_eq!(resolve_payee_in(&series, "prime").unwrap().1, None);
        // No match and ambiguous both error rather than guessing.
        assert!(resolve_payee_in(&series, "zzz").is_err());
        assert!(resolve_payee_in(&series, "p").is_err()); // spotify, peacock, prime
        assert!(resolve_payee_in(&series, "  ").is_err()); // empty query
    }

    #[test]
    fn bills_triage_lists_only_unconfirmed_and_respects_filter() {
        let mut a = test_app();
        a.view = View::Bills;
        a.data.recurring = Some(arca_core::rpc::RecurringPage {
            series: vec![
                labeled("spotify", "Spotify", Some("sub")), // confirmed -> excluded
                labeled("peacock", "Peacock", None),        // unconfirmed
                labeled("volt power", "Volt Power", None),  // unconfirmed
            ],
            ignored: vec![],
        });
        // Only the two unlabeled series, in their stored (soonest-due) order.
        let triage = a.bills_triage();
        assert_eq!(triage.len(), 2);
        assert_eq!(triage[0].series.payee, "peacock");
        assert_eq!(triage[1].series.payee, "volt power");
        // The `/` filter narrows the cursor set the same way it narrows the table.
        a.filter = "peacock".into();
        let triage = a.bills_triage();
        assert_eq!(triage.len(), 1);
        assert_eq!(triage[0].series.payee, "peacock");
    }

    #[test]
    fn bills_view_renders_triage_with_cursor_and_suggestions() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut a = test_app();
        a.view = View::Bills;
        a.data.recurring = Some(arca_core::rpc::RecurringPage {
            series: vec![
                labeled("spotify", "Spotify", None),       // suggest: sub
                labeled("volt power", "Volt Power", None), // suggest: bill
                labeled("corner gym", "Corner Gym", None), // suggest: none
            ],
            ignored: vec![],
        });
        a.bills_sel = 1; // cursor on a middle row exercises the highlight + window
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        // The Cell-based table, cursor highlight, suggest column and windowing must
        // render without panicking (e.g. on bounds or a zero-height pane).
        render(&mut term, &mut a).unwrap();
    }

    /// A confirmed series with a future charge graduates to the Schedule zone;
    /// unconfirmed ones stay in Triage. (`labeled` defaults predicted_next to 0,
    /// which is in the past, so the confirmed one needs a future date to graduate.)
    #[test]
    fn bills_zones_partition_confirmed_vs_unconfirmed() {
        let mut a = test_app();
        a.view = View::Bills;
        let future = now_secs() + 10 * 86_400;
        let mut sub = labeled("spotify", "Spotify", Some("sub"));
        sub.series.predicted_next = future;
        a.data.recurring = Some(arca_core::rpc::RecurringPage {
            series: vec![sub, labeled("peacock", "Peacock", None)],
            ignored: vec![],
        });
        assert_eq!(a.bills_triage().len(), 1);
        assert_eq!(a.bills_triage()[0].series.payee, "peacock");
        assert_eq!(a.bills_schedule().len(), 1);
        assert_eq!(a.bills_schedule()[0].series.payee, "spotify");
        // The active zone follows bills_zone.
        assert_eq!(a.bills_active()[0].series.payee, "peacock");
        a.bills_zone = BillsZone::Schedule;
        assert_eq!(a.bills_active()[0].series.payee, "spotify");
    }

    #[test]
    fn bills_menu_items_track_label_state() {
        let mut a = test_app();
        a.view = View::Bills;
        a.data.recurring = Some(arca_core::rpc::RecurringPage {
            series: vec![labeled("volt power", "Volt Power", None)],
            ignored: vec![],
        });
        // Unconfirmed: no Unlabel item; the inbuilt suggestion (bill) is pre-picked.
        a.open_bills_menu();
        let menu = a.bills_menu.as_ref().expect("menu open");
        assert!(
            !menu
                .items
                .iter()
                .any(|(act, _)| matches!(act, BillsAction::Unlabel))
        );
        assert_eq!(menu.items[menu.sel].1, "Bill");

        // Confirmed (Schedule zone): Unlabel offered, current label pre-picked.
        let mut a = test_app();
        a.view = View::Bills;
        a.bills_zone = BillsZone::Schedule;
        let future = now_secs() + 10 * 86_400;
        let mut sub = labeled("spotify", "Spotify", Some("sub"));
        sub.series.predicted_next = future;
        a.data.recurring = Some(arca_core::rpc::RecurringPage {
            series: vec![sub],
            ignored: vec![],
        });
        a.open_bills_menu();
        let menu = a.bills_menu.as_ref().expect("menu open");
        assert!(
            menu.items
                .iter()
                .any(|(act, _)| matches!(act, BillsAction::Unlabel))
        );
        assert_eq!(menu.items[menu.sel].1, "Subscription");
    }

    #[test]
    fn bills_view_renders_with_menu_open() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut a = test_app();
        a.view = View::Bills;
        a.data.recurring = Some(arca_core::rpc::RecurringPage {
            series: vec![labeled("volt power", "Volt Power", None)],
            ignored: vec![],
        });
        a.open_bills_menu();
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        // The Clear + centered popup overlay must render without panicking.
        render(&mut term, &mut a).unwrap();
    }

    #[test]
    fn bills_ignored_zone_offers_restore() {
        let mut a = test_app();
        a.view = View::Bills;
        a.bills_zone = BillsZone::Ignored;
        a.data.recurring = Some(arca_core::rpc::RecurringPage {
            series: vec![],
            ignored: vec![labeled("corner gym", "Corner Gym", Some("ignore"))],
        });
        // The Ignored zone selects from RecurringPage.ignored.
        assert_eq!(a.bills_ignored().len(), 1);
        assert_eq!(a.bills_active()[0].series.payee, "corner gym");
        // Its menu leads with Restore (pre-selected) and offers no plain "Ignore".
        a.open_bills_menu();
        let menu = a.bills_menu.as_ref().expect("menu open");
        assert_eq!(menu.items[menu.sel].1, "Restore (un-ignore)");
        assert!(menu.items.iter().all(|(_, t)| *t != "Ignore (hide)"));
    }

    #[test]
    fn bills_view_renders_ignored_zone() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut a = test_app();
        a.view = View::Bills;
        a.bills_zone = BillsZone::Ignored;
        a.data.recurring = Some(arca_core::rpc::RecurringPage {
            series: vec![],
            ignored: vec![labeled("corner gym", "Corner Gym", Some("ignore"))],
        });
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        render(&mut term, &mut a).unwrap();
    }

    #[test]
    fn bills_schedule_cursor_walks_all_rows_and_opens_declared_menu() {
        use arca_core::rpc::{DebtScheduled, DebtSnapshot, Scope};
        let mut a = test_app();
        a.view = View::Bills;
        a.bills_zone = BillsZone::Schedule;
        let future = now_secs() + 5 * 86_400;
        // A declared sub (Rent) rides the schedule via the debt snapshot's `fixed`.
        a.data.debt = Some(DebtSnapshot {
            scope: Scope::Month,
            open_balances: vec![],
            scheduled: vec![],
            fixed: vec![DebtScheduled {
                due_at: future,
                amount: Cents(-2_000_00),
                description: "Rent".into(),
            }],
            total_open: Cents(0),
        });
        // Plus one confirmed detected bill that sorts before Rent.
        let mut bill = labeled("aqua", "Aqua", Some("bill"));
        bill.series.predicted_next = future - 86_400;
        a.data.recurring = Some(arca_core::rpc::RecurringPage {
            series: vec![bill],
            ignored: vec![],
        });
        // The cursor walks BOTH rows (detected aqua + declared Rent), not just detected.
        assert_eq!(a.bills_zone_len(BillsZone::Schedule), 2);
        // On the declared row, Enter opens a Rename/Remove menu keyed by the sub name.
        a.bills_sel = 1;
        a.open_bills_menu();
        let menu = a.bills_menu.as_ref().expect("menu open");
        assert_eq!(menu.match_key, "Rent");
        let texts: Vec<&str> = menu.items.iter().map(|(_, t)| *t).collect();
        assert!(texts.contains(&"Rename..."));
        assert!(texts.iter().any(|t| t.starts_with("Remove")));
    }

    #[test]
    fn common_prefix_of_verbs() {
        // TAB on "" with several "label"-ish verbs extends to the shared prefix.
        assert_eq!(common_prefix(&["label ", "unlabel "]), "");
        assert_eq!(common_prefix(&["rename ", "refresh "]), "re");
        assert_eq!(common_prefix(&["ignore "]), "ignore ");
        assert_eq!(common_prefix(&[]), "");
        // Doesn't split a multibyte char mid-way.
        assert_eq!(common_prefix(&["café", "cama"]), "ca");
    }

    #[test]
    fn filter_matches_fuzzily() {
        assert!(hay_match("", "anything")); // empty filter = match all
        assert!(hay_match("aqua", "Aqua membership renewal"));
        assert!(hay_match("Power", "volt-power electric")); // substring still hits
        assert!(hay_match("aqmem", "Aqua membership")); // fuzzy subsequence
        assert!(hay_match("nflx", "Netflix")); // gaps allowed
        assert!(!hay_match("netflix", "Aqua membership renewal")); // no subsequence
        assert!(!hay_match("memaqa", "Aqua membership")); // order matters
    }

    #[test]
    fn scroll_clamps_to_content_bounds() {
        let mut a = test_app();
        a.view = View::Tx; // view-wide scroll path (not the Dashboard focus-box one)
        a.content_len = 100;
        a.viewport = 10;
        assert_eq!(a.max_scroll(), 90);

        a.scroll_by(5);
        assert_eq!(a.scroll, 5);
        a.scroll_by(-100); // can't go below the top
        assert_eq!(a.scroll, 0);
        a.scroll_by(1000); // can't go past the last full page
        assert_eq!(a.scroll, 90);
    }

    #[test]
    fn scroll_pinned_when_content_fits() {
        let mut a = test_app();
        a.view = View::Tx;
        a.content_len = 4;
        a.viewport = 20;
        assert_eq!(a.max_scroll(), 0);
        a.scroll_by(10);
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn invest_renders_wide() {
        use arca_core::pp::{Backbone, DriftRow};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let total = 149_250_00;
        let mk = |class: &str, cents: i64, breach: bool| DriftRow {
            asset_class: class.into(),
            actual_cents: Cents(cents),
            actual_pct: cents as f64 / total as f64 * 100.0,
            target_pct: 100.0 / 3.0,
            drift_pp: cents as f64 / total as f64 * 100.0 - 100.0 / 3.0,
            lower_band_pct: 22.0,
            upper_band_pct: 44.0,
            band_breach: breach,
        };
        let mut a = test_app();
        a.view = View::Pp;
        a.data.pp = Some(arca_core::rpc::PpSnapshot {
            rows: vec![
                mk("equity", 67_000_00, true),
                mk("long_treasuries", 44_250_00, false),
                mk("cash", 38_000_00, false),
            ],
            total: Cents(total),
            backbone: Backbone {
                gold: Cents(48_000_00),
                silver: Cents(9_400_00),
                xmr: Cents(12_300_00),
                land: Cents(85_000_00),
                sfr: Cents(0),
                other: Cents(0),
                total: Cents(154_700_00),
            },
            band_breach: true,
        });
        let backend = TestBackend::new(140, 30);
        let mut term = Terminal::new(backend).unwrap();
        render(&mut term, &mut a).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        // Side-by-side panes + the actionable rebalance call render with data.
        for needle in [
            "sleeves",
            "allocation",
            "T1 backbone",
            "BAND BREACH",
            "rebalance",
            "+$17.2k",
            "BREACH",
        ] {
            assert!(out.contains(needle), "invest missing {needle:?}");
        }
    }

    #[test]
    fn sleeve_drift_dollars_and_target() {
        use arca_core::pp::DriftRow;
        let total = 150_000_00;
        let over = DriftRow {
            asset_class: "equity".into(),
            actual_cents: Cents(67_000_00),
            actual_pct: 44.7,
            target_pct: 100.0 / 3.0,
            drift_pp: 11.3,
            lower_band_pct: 22.0,
            upper_band_pct: 44.0,
            band_breach: true,
        };
        // target = 33.3% of 150k = 50k; drift = 67k − 50k = +17k (overweight → sell).
        assert_eq!(sleeve_target_cents(&over, total), 50_000_00);
        assert_eq!(sleeve_drift_cents(&over, total), 17_000_00);
        // Untargeted sleeve (target_pct 0) → zero target, drift == actual.
        let gold = DriftRow {
            asset_class: "gold_etf".into(),
            actual_cents: Cents(5_000_00),
            actual_pct: 3.3,
            target_pct: 0.0,
            drift_pp: 0.0,
            lower_band_pct: 0.0,
            upper_band_pct: 0.0,
            band_breach: false,
        };
        assert_eq!(sleeve_target_cents(&gold, total), 0);
    }

    fn seed_charts(a: &mut App) {
        use arca_core::rpc::*;
        let now = now_secs();
        a.data.charts = Some(ChartsSnapshot {
            net_worth: vec![
                TimePoint {
                    at_secs: now - 60 * 86_400,
                    amount: Cents(300_000_00),
                },
                TimePoint {
                    at_secs: now - 30 * 86_400,
                    amount: Cents(308_730_00),
                },
                TimePoint {
                    at_secs: now,
                    amount: Cents(312_940_00),
                },
            ],
            cash_flow: vec![
                MonthFlow {
                    label: "2026-04".into(),
                    income: Cents(13_000_00),
                    expenses: Cents(-9_400_00),
                    net: Cents(3_600_00),
                },
                MonthFlow {
                    label: "2026-05".into(),
                    income: Cents(14_000_00),
                    expenses: Cents(-11_860_00),
                    net: Cents(2_140_00),
                },
            ],
        });
        a.data.categories = Some(CategoriesSnapshot {
            scope: Scope::Month,
            since: now - 30 * 86_400,
            until: now,
            rows: vec![
                CategorySpend {
                    category: "Rent".into(),
                    amount: Cents(-2_000_00),
                },
                CategorySpend {
                    category: "Groceries".into(),
                    amount: Cents(-840_00),
                },
                CategorySpend {
                    category: "Transport".into(),
                    amount: Cents(-310_00),
                },
            ],
            total: Cents(-3_150_00),
        });
    }

    #[test]
    fn charts_renders_wide() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut a = test_app();
        a.view = View::Charts;
        seed_charts(&mut a);
        let backend = TestBackend::new(140, 30);
        let mut term = Terminal::new(backend).unwrap();
        render(&mut term, &mut a).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        // All three series render: trend + cash flow + the folded-in spend pane.
        for needle in [
            "net worth over time",
            "monthly cash flow",
            "top categories",
            "Rent",
            "Groceries",
        ] {
            assert!(out.contains(needle), "charts missing {needle:?}");
        }
    }

    #[test]
    fn bills_renders_wide() {
        use arca_core::rpc::*;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let now = now_secs();
        let mut a = test_app();
        a.view = View::Bills;
        a.data.debt = Some(DebtSnapshot {
            scope: Scope::Month,
            open_balances: vec![
                DebtBalance {
                    account_name: "Example Card".into(),
                    balance: Cents(12_300_00),
                },
                DebtBalance {
                    account_name: "Truck loan".into(),
                    balance: Cents(6_000_00),
                },
            ],
            scheduled: vec![DebtScheduled {
                due_at: now + 4 * 86_400,
                amount: Cents(-450_00),
                description: "Card pmt".into(),
            }],
            // Declared recurring obligation (rent) — fixed band, not debt.
            fixed: vec![DebtScheduled {
                due_at: now + 9 * 86_400,
                amount: Cents(-2_000_00),
                description: "Rent".into(),
            }],
            total_open: Cents(18_300_00),
        });
        // One labeled bill (in-window so it lands in "next 30d") + two unlabeled.
        let mut bill = labeled("aqua", "Aqua Membership", Some("bill"));
        bill.series.predicted_next = now + 6 * 86_400;
        bill.series.avg_amount = Cents(-220_00);
        a.data.recurring = Some(RecurringPage {
            series: vec![
                bill,
                labeled("netflix", "Netflix", None),
                labeled("spotify", "Spotify", None),
            ],
            ignored: vec![],
        });
        let backend = TestBackend::new(140, 30);
        let mut term = Terminal::new(backend).unwrap();
        render(&mut term, &mut a).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        // Status band rollup + both debt-detail panes + the recurring table render
        // with data (not placeholders).
        for needle in [
            "open balances",
            "scheduled", // pane title " scheduled · recurring "
            "recurring",
            "next 30d",
            "Example Card",
            "Card pmt", // scheduled debt service
            "Rent",     // declared recurring obligation (fixed band)
            "1 bill",
            "2 unlabeled",
        ] {
            assert!(out.contains(needle), "bills missing {needle:?}");
        }
    }

    #[test]
    fn accounts_view_lists_individual_assets() {
        use arca_core::rpc::*;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let now = now_secs();
        let mut a = test_app();
        a.view = View::Money;
        a.data.money = Some(MoneySnapshot {
            net_worth: Cents(16_910_00),
            by_kind: vec![KindTotal {
                kind: "asset".into(),
                total: Cents(16_910_00),
                account_count: 2,
            }],
            accounts: vec![
                AccountLine {
                    name: "First Bank Checking".into(),
                    kind: "asset".into(),
                    balance: Cents(10_000_00),
                    currency: "USD".into(),
                    excluded_from_nw: false,
                },
                AccountLine {
                    name: "Second Bank Savings".into(),
                    kind: "asset".into(),
                    balance: Cents(6_910_00),
                    currency: "USD".into(),
                    excluded_from_nw: false,
                },
                // Non-USD: listed under its own header, excluded from net worth.
                AccountLine {
                    name: "Monero Wallet".into(),
                    kind: "brokerage".into(),
                    balance: Cents(42),
                    currency: "XMR".into(),
                    excluded_from_nw: true,
                },
            ],
            subscriptions: vec![],
            asof_secs: now,
        });
        let backend = TestBackend::new(140, 30);
        let mut term = Terminal::new(backend).unwrap();
        render(&mut term, &mut a).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        // Each asset listed by name (not just the aggregate), plus the non-USD
        // orphan-kind account under its own header.
        for needle in [
            "First Bank Checking",
            "Second Bank Savings",
            "Monero Wallet",
            "XMR",
            "excluded from net worth",
        ] {
            assert!(out.contains(needle), "accounts missing {needle:?}");
        }
    }

    #[test]
    fn menu_renders_with_data_wide() {
        use arca_core::rpc::*;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let now = now_secs();
        let mut a = test_app();
        a.view = View::Menu;
        a.data.money = Some(MoneySnapshot {
            net_worth: Cents(312_940_00),
            by_kind: vec![
                KindTotal {
                    kind: "asset".into(),
                    total: Cents(31_000_00),
                    account_count: 2,
                },
                KindTotal {
                    kind: "business".into(),
                    total: Cents(45_000_00),
                    account_count: 1,
                },
                KindTotal {
                    kind: "brokerage".into(),
                    total: Cents(149_000_00),
                    account_count: 2,
                },
            ],
            accounts: vec![],
            subscriptions: vec![],
            asof_secs: now,
        });
        a.data.charts = Some(ChartsSnapshot {
            net_worth: vec![
                TimePoint {
                    at_secs: now - 30 * 86_400,
                    amount: Cents(308_730_00),
                },
                TimePoint {
                    at_secs: now,
                    amount: Cents(312_940_00),
                },
            ],
            cash_flow: vec![MonthFlow {
                label: "2026-05".into(),
                income: Cents(14_000_00),
                expenses: Cents(-11_860_00),
                net: Cents(2_140_00),
            }],
        });
        a.data.debt = Some(DebtSnapshot {
            scope: Scope::Month,
            open_balances: vec![
                DebtBalance {
                    account_name: "Example Card".into(),
                    balance: Cents(12_300_00),
                },
                DebtBalance {
                    account_name: "Truck loan".into(),
                    balance: Cents(6_000_00),
                },
            ],
            scheduled: vec![DebtScheduled {
                due_at: now + 4 * 86_400,
                amount: Cents(1_800_00),
                description: "Rent".into(),
            }],
            fixed: vec![],
            total_open: Cents(18_300_00),
        });
        let mut bill = labeled("aqua", "Aqua Membership", Some("bill"));
        bill.series.predicted_next = now + 6 * 86_400;
        bill.series.avg_amount = Cents(-220_00);
        a.data.recurring = Some(RecurringPage {
            series: vec![
                bill,
                labeled("netflix", "Netflix", None),
                labeled("spotify", "Spotify", None),
            ],
            ignored: vec![],
        });
        a.data.alerts = Some(AlertsPage {
            rows: vec![AlertRow {
                id: 1,
                rule_name: "plaid".into(),
                rule_kind: Some("provider.stale".into()),
                fired_at: now,
                delivered: false,
                summary: "plaid stale".into(),
            }],
            rules: vec![],
        });
        let descs = [
            "Stripe payout",
            "Uber",
            "Costco",
            "Netflix",
            "TreasuryDirect",
            "Shell",
            "Aqua Membership",
            "Amazon",
            "Mercury fee",
            "Spotify",
            "Walmart",
            "Delta",
        ];
        a.data.tx = Some(TxListPage {
            rows: descs
                .iter()
                .enumerate()
                .map(|(i, d)| TxRow {
                    id: i as i64,
                    posted_at: now - i as i64 * 86_400,
                    account: "First Bank Checking".into(),
                    amount: Cents(if i == 0 {
                        120_400
                    } else {
                        -(20_00 + i as i64 * 1_137)
                    }),
                    description: Some((*d).to_string()),
                    category: None,
                    tag: Some(if i == 0 { "income" } else { "expense" }.into()),
                })
                .collect(),
        });
        let backend = TestBackend::new(140, 40);
        let mut term = Terminal::new(backend).unwrap();
        render(&mut term, &mut a).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        // The wide layout shows the launcher + both right-hand panels, and the
        // teasers/Recent rows render their data (not just placeholders).
        for needle in [
            "menu",
            "recent",
            "upcoming",
            "Accounts",
            "net worth by kind",
            "this month",
            "alerts pending",
            "Stripe payout",
            "Rent",
            "$18.3k",
        ] {
            assert!(out.contains(needle), "menu missing {needle:?}");
        }
    }

    #[test]
    fn menu_move_clamps_to_the_destination_list() {
        // Cursor walks the tiles and clamps at both ends (no wrap).
        let mut a = test_app(); // App::new → view = Menu, menu_sel = 0
        a.menu_move(-1); // already at the top → stays
        assert_eq!(a.menu_sel, 0);
        a.menu_move(1);
        assert_eq!(a.menu_sel, 1);
        a.menu_move(100); // clamps to the last tile
        assert_eq!(a.menu_sel, MENU_ITEMS.len() - 1);
        a.menu_move(1); // already at the bottom → stays
        assert_eq!(a.menu_sel, MENU_ITEMS.len() - 1);
    }

    #[test]
    fn menu_items_letters_are_unique() {
        // The tile letters double as global jump keys, so they must not collide.
        let mut keys: Vec<char> = MENU_ITEMS.iter().map(|(k, _, _)| *k).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), MENU_ITEMS.len());
    }

    #[test]
    fn month_abbr_maps_and_falls_back() {
        assert_eq!(month_abbr("2026-01"), "Jan");
        assert_eq!(month_abbr("2026-12"), "Dec");
        assert_eq!(month_abbr("2026-13"), "2026-13"); // out of range → raw
        assert_eq!(month_abbr("garbage"), "garbage");
    }

    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate("short", 18), "short");
        assert_eq!(truncate("abcdef", 4), "ab..");
        // multibyte must not panic on a char boundary
        assert_eq!(truncate("café société", 5), "caf..");
        // glyphs the console font can't render are folded to ASCII (em-dash in
        // provider account names like "OpenRouter — main") rather than shown raw.
        assert_eq!(truncate("OpenRouter — main", 30), "OpenRouter - main");
        assert_eq!(ascii_glyph("a—b…c⚠"), "a-b...c!");
    }

    #[test]
    fn money_short_scales() {
        assert_eq!(money_short(430.0), "$430");
        assert_eq!(money_short(17_037.73), "$17.0k");
        assert_eq!(money_short(1_500_000.0), "$1.5M");
    }

    #[test]
    fn nice_yticks_rounds_and_scales() {
        let few = nice_yticks(0.0, 16_942.0, 2);
        let many = nice_yticks(0.0, 16_942.0, 8);
        // taller pane (bigger target) → at least as many ticks
        assert!(many.len() >= few.len());
        assert!(few.len() >= 2);
        // bounds bracket the data
        assert!(*few.first().unwrap() <= 0.0 && *few.last().unwrap() >= 16_942.0);
        // evenly spaced by a round step
        let step = many[1] - many[0];
        assert!(many.windows(2).all(|w| (w[1] - w[0] - step).abs() < 1e-6));
        // flat data never degenerates or panics
        let flat = nice_yticks(500.0, 500.0, 5);
        assert!(flat.len() >= 2 && flat.first().unwrap() < flat.last().unwrap());
    }

    #[test]
    fn month_start_unix_anchors() {
        // Known epoch anchors (UTC midnight, 1st of month).
        assert_eq!(month_start_unix("1970-01"), Some(0));
        assert_eq!(month_start_unix("2000-01"), Some(946_684_800));
        // Window upper bound = start of the following month.
        assert_eq!(next_month_start_unix("1970-01"), Some(2_678_400)); // +31d
        assert_eq!(
            next_month_start_unix("2026-12"),
            month_start_unix("2027-01")
        );
        // Honest failure on malformed labels.
        assert_eq!(month_start_unix("bad"), None);
        assert_eq!(month_start_unix("2026-13"), None);
    }
}
