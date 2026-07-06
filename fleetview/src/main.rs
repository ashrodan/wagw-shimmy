//! fleetview — a terminal admin for the wagw shim fleet.
//!
//! Reads the live deploy config (`deploy/fleet.yaml` + `deploy/tenants/*.yaml`) and shows, per
//! WhatsApp number: who is allowed to reach it, how each conversation is routed through the shim to
//! a downstream agent channel, and any structurally-derived config gaps. Read-only; press `r` to
//! re-read from disk.
//!
//!   cargo run -p fleetview                 # auto-locates ./deploy
//!   cargo run -p fleetview -- path/to/deploy
//!
//! Layout: a tenant list on the left, a scrollable detail panel on the right, a summary header and
//! a key-hint footer. Colour comes from the terminal's own palette (works in light or dark).

mod model;

use std::io;
use std::path::PathBuf;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use ratatui::{DefaultTerminal, Frame};

use model::{Fleet, Sev, Tenant};

const ACCENT: Color = Color::Cyan;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dump = args.iter().any(|a| a == "--dump");
    let path_arg = args.iter().find(|a| !a.starts_with("--")).cloned();

    let deploy_dir = match resolve_deploy_dir(path_arg) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fleetview: {e}");
            eprintln!("  pass the deploy directory explicitly:  fleetview <path-to-deploy>");
            std::process::exit(2);
        }
    };

    let fleet = match Fleet::load(&deploy_dir) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("fleetview: failed to read {}: {e}", deploy_dir.display());
            std::process::exit(1);
        }
    };

    if dump {
        dump_plain(&fleet);
        return;
    }

    let mut app = App::new(fleet);
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    if let Err(e) = result {
        eprintln!("fleetview: {e}");
        std::process::exit(1);
    }
}

/// Find the `deploy/` directory: explicit argv, else `$FLEETVIEW_DEPLOY_DIR`, else walk up from the
/// cwd looking for a `deploy/fleet.yaml` (so it works whether run from the repo root or a subdir).
fn resolve_deploy_dir(path_arg: Option<String>) -> io::Result<PathBuf> {
    if let Some(arg) = path_arg {
        let p = PathBuf::from(arg);
        return if p.join("fleet.yaml").exists() || p.join("tenants").is_dir() {
            Ok(p)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no fleet.yaml/tenants under {}", p.display()),
            ))
        };
    }
    if let Ok(env) = std::env::var("FLEETVIEW_DEPLOY_DIR") {
        return Ok(PathBuf::from(env));
    }
    let mut dir = std::env::current_dir()?;
    loop {
        let candidate = dir.join("deploy");
        if candidate.join("fleet.yaml").exists() {
            return Ok(candidate);
        }
        if dir.join("fleet.yaml").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "could not locate a deploy/ directory (no deploy/fleet.yaml found walking up from cwd)",
            ));
        }
    }
}

struct App {
    fleet: Fleet,
    list: ListState,
    scroll: u16,
    content_lines: u16,
    viewport_lines: u16,
    should_quit: bool,
    status: Option<String>,
}

impl App {
    fn new(fleet: Fleet) -> Self {
        let mut list = ListState::default();
        if !fleet.tenants.is_empty() {
            list.select(Some(0));
        }
        App {
            fleet,
            list,
            scroll: 0,
            content_lines: 0,
            viewport_lines: 0,
            should_quit: false,
            status: None,
        }
    }

    fn selected(&self) -> Option<&Tenant> {
        self.list.selected().and_then(|i| self.fleet.tenants.get(i))
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.should_quit {
            terminal.draw(|f| self.render(f))?;
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k.code, k.modifiers),
                _ => {}
            }
        }
        Ok(())
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let max_scroll = self.content_lines.saturating_sub(self.viewport_lines);
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.should_quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.select_delta(1),
            KeyCode::Up | KeyCode::Char('k') => self.select_delta(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.scroll = (self.scroll + 10).min(max_scroll)
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = max_scroll,
            KeyCode::Char('r') => self.reload(),
            _ => {}
        }
    }

    fn select_delta(&mut self, delta: i32) {
        let n = self.fleet.tenants.len();
        if n == 0 {
            return;
        }
        let cur = self.list.selected().unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(n as i32) as usize;
        self.list.select(Some(next));
        self.scroll = 0;
    }

    fn reload(&mut self) {
        let dir = self.fleet.deploy_dir.clone();
        match Fleet::load(&dir) {
            Ok(f) => {
                let keep = self
                    .list
                    .selected()
                    .unwrap_or(0)
                    .min(f.tenants.len().saturating_sub(1));
                self.fleet = f;
                self.list
                    .select((!self.fleet.tenants.is_empty()).then_some(keep));
                self.scroll = 0;
                self.status = Some("reloaded from disk".into());
            }
            Err(e) => self.status = Some(format!("reload failed: {e}")),
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        self.render_header(frame, header);

        let [left, right] =
            Layout::horizontal([Constraint::Length(26), Constraint::Min(20)]).areas(body);
        self.render_list(frame, left);
        self.render_detail(frame, right);

        self.render_footer(frame, footer);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let s = self.fleet.summary();
        let tile = |label: &'static str, val: String, c: Color| {
            vec![
                Span::styled(val, Style::new().fg(c).add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                Span::styled(label, Style::new().fg(Color::DarkGray)),
            ]
        };
        let mut spans = vec![
            Span::styled(
                "wagw ",
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("shim fleet", Style::new().fg(Color::Gray)),
            Span::raw("      "),
        ];
        spans.extend(tile("tenants", s.tenants.to_string(), ACCENT));
        spans.push(Span::raw("   "));
        spans.extend(tile("live", s.live.to_string(), Color::Green));
        spans.push(Span::raw("   "));
        spans.extend(tile("targets", s.targets.to_string(), ACCENT));
        spans.push(Span::raw("   "));
        spans.extend(tile("groups", s.groups.to_string(), ACCENT));
        spans.push(Span::raw("   "));
        spans.extend(tile(
            "gaps",
            s.gaps.to_string(),
            if s.gaps > 0 {
                Color::Yellow
            } else {
                Color::Green
            },
        ));

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(
                " WhatsApp gateway — live deploy config ",
                Style::new().fg(Color::Gray),
            ));
        frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .fleet
            .tenants
            .iter()
            .map(|t| {
                let (glyph, c) = sev_glyph(t.status_sev);
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {glyph} "), Style::new().fg(c)),
                    Span::styled(t.id.clone(), Style::new().add_modifier(Modifier::BOLD)),
                ]))
            })
            .collect();

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(" numbers ", Style::new().fg(Color::Gray)));
        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::new()
                    .bg(ACCENT)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("");
        frame.render_stateful_widget(list, area, &mut self.list);
    }

    fn render_detail(&mut self, frame: &mut Frame, area: Rect) {
        let title = self
            .selected()
            .map(|t| format!(" {} ", t.id))
            .unwrap_or_else(|| " — ".into());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::DarkGray))
            .padding(Padding::horizontal(1))
            .title(Span::styled(
                title,
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));

        let inner = block.inner(area);
        let lines = self.selected().map(detail_lines).unwrap_or_default();

        self.content_lines = lines.len() as u16;
        self.viewport_lines = inner.height;
        let max_scroll = self.content_lines.saturating_sub(self.viewport_lines);
        self.scroll = self.scroll.min(max_scroll);

        let para = Paragraph::new(lines).block(block).scroll((self.scroll, 0));
        frame.render_widget(para, area);

        if max_scroll > 0 {
            let mut sb = ScrollbarState::new(max_scroll as usize).position(self.scroll as usize);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area,
                &mut sb,
            );
        }
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let hint = |k: &'static str, d: &'static str| {
            vec![
                Span::styled(k, Style::new().fg(ACCENT)),
                Span::styled(format!(" {d}  "), Style::new().fg(Color::DarkGray)),
            ]
        };
        let mut spans = vec![Span::raw(" ")];
        spans.extend(hint("↑/↓", "number"));
        spans.extend(hint("PgUp/PgDn", "scroll"));
        spans.extend(hint("r", "reload"));
        spans.extend(hint("q", "quit"));
        if let Some(s) = &self.status {
            spans.push(Span::styled(
                format!("· {s}"),
                Style::new().fg(Color::Green),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn sev_color(sev: Sev) -> Color {
    match sev {
        Sev::Good => Color::Green,
        Sev::Warn => Color::Yellow,
        Sev::Crit => Color::Red,
        Sev::Info => ACCENT,
        Sev::Muted => Color::DarkGray,
    }
}

fn sev_glyph(sev: Sev) -> (char, Color) {
    (
        match sev {
            Sev::Good => '●',
            Sev::Warn => '◐',
            Sev::Crit => '○',
            _ => '·',
        },
        sev_color(sev),
    )
}

// ------------------------------------------------------- detail line building

fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_uppercase(),
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ))
}

fn kv(key: &str, val: String, c: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<16}"), Style::new().fg(Color::DarkGray)),
        Span::styled(val, Style::new().fg(c)),
    ])
}

fn detail_lines(t: &Tenant) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    // header
    let mut head = vec![
        Span::styled(
            t.id.clone(),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{}]", t.status),
            Style::new().fg(sev_color(t.status_sev)),
        ),
    ];
    if !t.box_host.is_empty() {
        head.push(Span::styled(
            format!("  {}", t.box_host),
            Style::new().fg(Color::Gray),
        ));
    }
    out.push(Line::from(head));
    let mut ident = Vec::new();
    if !t.device_jid.is_empty() {
        ident.push(format!("device_jid {}", t.device_jid));
    }
    if !t.gowa_device_id.is_empty() {
        ident.push(format!("device_id {}", t.gowa_device_id));
    }
    if !t.self_number.is_empty() {
        ident.push(format!("self {}", t.self_number));
    }
    if !t.magicdns.is_empty() {
        ident.push(format!("magicdns {}", t.magicdns));
    }
    if !ident.is_empty() {
        out.push(Line::from(Span::styled(
            ident.join("  ·  "),
            Style::new().fg(Color::DarkGray),
        )));
    }
    out.push(Line::raw(""));

    // routing pipeline
    out.push(heading("routing"));
    let rows = t.channel_rows();
    let dev = if !t.device_jid.is_empty() {
        t.device_jid.clone()
    } else if !t.wa_account.is_empty() {
        t.wa_account.clone()
    } else {
        "unpaired".into()
    };
    out.push(Line::from(vec![
        Span::styled(
            "WhatsApp ",
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{dev} "), Style::new().fg(Color::DarkGray)),
        Span::styled("─▶ ", Style::new().fg(Color::DarkGray)),
        Span::raw("GOWA "),
        Span::styled(":3000 ", Style::new().fg(Color::DarkGray)),
        Span::styled("─▶ ", Style::new().fg(Color::DarkGray)),
        Span::styled(
            "wagw-shimmy ",
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(":8080 policy·dedup·queue", Style::new().fg(Color::DarkGray)),
    ]));
    let n = rows.len();
    for (i, r) in rows.iter().enumerate() {
        let branch = if n == 1 {
            "  └▶ "
        } else if i == 0 {
            "  ├▶ "
        } else if i == n - 1 {
            "  └▶ "
        } else {
            "  ├▶ "
        };
        let label_color = if r.sink {
            Color::Red
        } else if r.implicit {
            Color::Gray
        } else {
            ACCENT
        };
        let serves = if r.sink {
            "sink — dropped".to_string()
        } else {
            r.serves.clone()
        };
        out.push(Line::from(vec![
            Span::styled(branch, Style::new().fg(Color::DarkGray)),
            Span::styled(
                format!("{:<8}", r.label),
                Style::new().fg(label_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<38}", if r.url.is_empty() { "—" } else { &r.url }),
                Style::new().fg(Color::Cyan),
            ),
            Span::styled(serves, Style::new().fg(Color::DarkGray)),
        ]));
    }
    out.push(Line::raw(""));

    // policy
    out.push(heading("who can reach this number"));
    let dm = t.dm_policy();
    let dm_sev = match dm {
        "open" => Sev::Warn,
        "off" => Sev::Muted,
        _ if t.dm_allow().is_empty() => Sev::Warn,
        _ => Sev::Good,
    };
    let dm_val = if dm == "allowlist" {
        format!("{dm} · {}", t.dm_allow().len())
    } else {
        dm.to_string()
    };
    out.push(kv("direct messages", dm_val, sev_color(dm_sev)));
    if t.dm_allow().is_empty() {
        out.push(Line::from(Span::styled(
            "                (none admitted)",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    } else {
        out.push(Line::from(Span::styled(
            format!("                {}", t.dm_allow().join(", ")),
            Style::new().fg(Color::Gray),
        )));
    }
    let grp = t.group_policy();
    let grp_val = if grp == "allowlist" {
        format!("{grp} · {}", t.routing_rows().len())
    } else {
        grp.to_string()
    };
    let grp_sev = match grp {
        "open" => Sev::Warn,
        "off" => Sev::Muted,
        _ => Sev::Good,
    };
    out.push(kv("groups", grp_val, sev_color(grp_sev)));
    out.push(kv(
        "require mention",
        if t.require_mention() {
            "on".into()
        } else {
            "off".into()
        },
        if t.require_mention() {
            ACCENT
        } else {
            Color::Yellow
        },
    ));
    out.push(kv(
        "send cap",
        format!("{}/min", t.send_rate_per_min()),
        Color::Gray,
    ));
    out.push(Line::raw(""));

    // groups & routing
    out.push(heading("groups & routing"));
    let routing = t.routing_rows();
    if routing.is_empty() {
        out.push(Line::from(Span::styled(
            "  (no groups configured)",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    } else {
        for (jid, ch, via) in &routing {
            let name = t.group_label(jid).unwrap_or("");
            let mut spans = vec![Span::raw("  ")];
            if !name.is_empty() {
                spans.push(Span::styled(
                    format!("{name}  "),
                    Style::new().fg(Color::Gray),
                ));
            }
            spans.push(Span::styled(
                format!("{jid:<26}"),
                Style::new().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(
                format!(" {ch:<8}"),
                Style::new()
                    .fg(if ch == "default" { Color::Gray } else { ACCENT })
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" {via}"),
                Style::new().fg(Color::DarkGray),
            ));
            out.push(Line::from(spans));
        }
    }
    out.push(Line::raw(""));

    // channels detail
    out.push(heading("channels & downstream targets"));
    out.push(Line::from(Span::styled(
        format!("  {:<8} {:<38} {}", "channel", "agent url", "inbound token"),
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::UNDERLINED),
    )));
    for r in &rows {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{:<8}", r.label),
                Style::new()
                    .fg(if r.implicit { Color::Gray } else { ACCENT })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {:<38}", if r.url.is_empty() { "—" } else { &r.url }),
                Style::new().fg(Color::Cyan),
            ),
            Span::styled(r.token.clone(), Style::new().fg(Color::DarkGray)),
        ]));
    }
    out.push(Line::raw(""));

    // secrets
    if !t.secrets.is_empty() {
        out.push(heading(
            "secrets · names only (values live in the store / on-box)",
        ));
        let names: Vec<String> = t.secrets.iter().map(|(k, _)| k.clone()).collect();
        out.push(Line::from(Span::styled(
            format!("  {}", names.join("  ")),
            Style::new().fg(Color::DarkGray),
        )));
        out.push(Line::raw(""));
    }

    // health
    let health = t.health();
    if !health.is_empty() {
        out.push(heading("health & gaps"));
        for (sev, msg) in &health {
            let (glyph, c) = sev_glyph(*sev);
            out.push(Line::from(vec![
                Span::styled(format!("  {glyph} "), Style::new().fg(c)),
                Span::styled(msg.clone(), Style::new().fg(Color::Gray)),
            ]));
        }
        out.push(Line::raw(""));
    }

    out.push(Line::from(Span::styled(
        format!("source · {}", t.source),
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));
    out
}

/// Non-interactive rendering: flatten every tenant's detail to plain text on stdout. Useful in a
/// pipe / CI (`fleetview --dump`) and when there's no TTY to drive the full UI.
fn dump_plain(fleet: &Fleet) {
    let s = fleet.summary();
    println!(
        "wagw shim fleet — tenants {} · live {} · targets {} · groups {} · gaps {}\n",
        s.tenants, s.live, s.targets, s.groups, s.gaps
    );
    for t in &fleet.tenants {
        for line in detail_lines(t) {
            let text: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
            println!("{text}");
        }
        println!("{}", "─".repeat(60));
    }
}
