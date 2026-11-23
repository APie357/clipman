// SPDX-License-Identifier: Apache-2.0
//! The Lua side of plugins: loading, the `clipman` API table, and calling into plugins
//! with errors and runaway code contained.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use eframe::egui;
use mlua::serde::{DeserializeOptions, SerializeOptions};
use mlua::{FromLua, Function, HookTriggers, Lua, LuaSerdeExt, Table, Value, VmState};

use super::*;
use crate::render::{Extra, Job};
use crate::settings::{PluginCfg, Settings};

/// How long one callback may run. Window drawing runs every frame, so it gets less.
const DRAW_BUDGET: Duration = Duration::from_millis(250);
const CALL_BUDGET: Duration = Duration::from_secs(2);
const LOAD_BUDGET: Duration = Duration::from_secs(5);

/// Requests plugins make through the `clipman` API, handled by the host after each call.
#[derive(Default)]
struct Shared {
    clip_cmds: Vec<ClipCmd>,
    /// (plugin id, window id, open)
    windows: Vec<(String, String, bool)>,
    notices: Vec<String>,
    spawns: Vec<SpawnReq>,
    clips_folder: PathBuf,
    export_folder: PathBuf,
    gui: bool,
}

struct SpawnReq {
    plugin: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    stdin: Option<String>,
    on_exit: Option<Function>,
}

struct ProcResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

struct Running {
    plugin: String,
    rx: Receiver<ProcResult>,
    on_exit: Option<Function>,
}

struct MenuDef {
    label: String,
    shortcut: Option<egui::KeyboardShortcut>,
    screen: Option<Screen>,
    run: Function,
}

/// A plugin's own dropdown; items refer to `Plugin::menu` by index.
struct MenuTree {
    title: String,
    entries: Vec<Entry>,
}

enum Entry {
    Item(usize),
    Separator,
    Sub(String, Vec<Entry>),
}

/// How deep submenus may nest.
const MAX_MENU_DEPTH: usize = 4;

struct ToolDef {
    label: String,
    tooltip: String,
    run: Function,
}

struct WindowDef {
    id: String,
    title: String,
    width: Option<f32>,
    height: Option<f32>,
    draw: Function,
    open: bool,
}

struct ActionDef {
    id: String,
    label: String,
    file_manager: bool,
    run: Function,
}

#[derive(Default)]
struct Hooks {
    clip_opened: Option<Function>,
    export_start: Option<Function>,
    export_done: Option<Function>,
    browser_refresh: Option<Function>,
    badge: Option<Function>,
}

struct Plugin {
    id: String,
    dir: PathBuf,
    name: String,
    version: String,
    description: String,
    /// Enabled in the settings when it was loaded.
    enabled: bool,
    lua: Option<Lua>,
    error: Option<String>,
    /// When the running callback has to stop (checked by an instruction hook).
    deadline: Rc<Cell<Option<Instant>>>,
    menu: Vec<MenuDef>,
    menus: Vec<MenuTree>,
    toolbar: Vec<ToolDef>,
    windows: Vec<WindowDef>,
    settings: Vec<SettingDef>,
    actions: Vec<ActionDef>,
    hooks: Hooks,
}

pub struct Host {
    plugins: Vec<Plugin>,
    shared: Rc<RefCell<Shared>>,
    running: Vec<Running>,
    /// False with `--no-plugins`: nothing is loaded, ever.
    active: bool,
    dir: PathBuf,
}

impl Host {
    /// No plugins (`--no-plugins`).
    pub fn none() -> Self {
        Self {
            plugins: Vec::new(),
            shared: Default::default(),
            running: Vec::new(),
            active: false,
            dir: PathBuf::new(),
        }
    }

    /// Load the plugins in `<config>/plugins`.
    pub fn load(settings: &Settings) -> Self {
        Self::load_from(crate::settings::plugins_dir(), settings)
    }

    pub fn load_from(dir: PathBuf, settings: &Settings) -> Self {
        let mut h = Self {
            active: true,
            dir,
            ..Self::none()
        };
        h.reload(settings);
        h
    }

    /// Whether `open_window` shows anything (false for command-line renders).
    pub fn set_gui(&mut self, gui: bool) {
        self.shared.borrow_mut().gui = gui;
    }

    /// Drop every plugin and load them again (their windows close, running processes
    /// are forgotten).
    pub fn reload(&mut self, settings: &Settings) {
        self.plugins.clear();
        self.running.clear();
        if !self.active {
            return;
        }
        self.set_folders(settings);
        let mut dirs: Vec<(String, PathBuf)> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("init.lua").is_file())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect();
        dirs.sort();
        for (id, dir) in dirs {
            let enabled = settings.plugin_enabled(&id);
            let mut p = Plugin::new(id, dir, enabled);
            if !valid_id(&p.id) {
                p.error =
                    Some("the folder name must be lowercase letters, digits, - and _ only".into());
            } else if enabled && let Err(e) = p.load(&self.shared, settings.plugins.get(&p.id)) {
                let msg = format!("{e:#}");
                eprintln!("clipman: plugin {} didn't load: {msg}", p.id);
                p.error = Some(msg);
                p.lua = None;
            }
            self.plugins.push(p);
        }
        self.flush();
    }

    /// New settings were saved: reload if plugins were turned on or off, otherwise just
    /// pass the plugins their new setting values.
    pub fn apply_settings(&mut self, settings: &Settings) {
        if !self.active {
            return;
        }
        if self
            .plugins
            .iter()
            .any(|p| p.enabled != settings.plugin_enabled(&p.id))
        {
            self.reload(settings);
            return;
        }
        self.set_folders(settings);
        for p in &mut self.plugins {
            let cfg = settings.plugins.get(&p.id);
            if let Some(lua) = &p.lua
                && let Err(e) = set_settings_table(lua, &p.settings, cfg)
            {
                p.error = Some(e.to_string());
            }
        }
    }

    fn set_folders(&mut self, settings: &Settings) {
        let mut sh = self.shared.borrow_mut();
        sh.clips_folder = settings.clips_folder();
        sh.export_folder = settings.export_folder();
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn infos(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|p| PluginInfo {
                id: p.id.clone(),
                name: p.name.clone(),
                version: p.version.clone(),
                description: p.description.clone(),
                error: p.error.clone(),
                enabled: p.enabled,
                settings: p.settings.clone(),
            })
            .collect()
    }

    fn live(&self) -> impl Iterator<Item = (usize, &Plugin)> {
        self.plugins
            .iter()
            .enumerate()
            .filter(|(_, p)| p.lua.is_some() && p.error.is_none())
    }

    /// Every menu item with a shortcut that works on `screen`.
    pub fn shortcut_items(&self, screen: Screen) -> Vec<MenuItem> {
        let mut out = Vec::new();
        for (pi, p) in self.live() {
            for i in 0..p.menu.len() {
                if p.menu[i].shortcut.is_some()
                    && let Some(it) = p.item(pi, i, screen)
                {
                    out.push(it);
                }
            }
        }
        out
    }

    /// The plugins' own dropdowns for this screen (empty ones left out), merged by title.
    pub fn menus(&self, screen: Screen) -> Vec<PluginMenu> {
        let mut out: Vec<PluginMenu> = Vec::new();
        for (pi, p) in self.live() {
            for tree in &p.menus {
                let entries = p.entries(pi, &tree.entries, screen);
                if entries.is_empty() {
                    continue;
                }
                match out.iter_mut().find(|m| m.title == tree.title) {
                    Some(m) => {
                        m.entries.push(MenuEntry::Separator);
                        m.entries.extend(entries);
                    }
                    None => out.push(PluginMenu {
                        title: tree.title.clone(),
                        entries,
                    }),
                }
            }
        }
        out
    }

    pub fn toolbar_items(&self) -> Vec<ToolItem> {
        let mut out = Vec::new();
        for (pi, p) in self.live() {
            for (i, t) in p.toolbar.iter().enumerate() {
                out.push(ToolItem {
                    plugin: pi,
                    item: i,
                    label: t.label.clone(),
                    tooltip: t.tooltip.clone(),
                });
            }
        }
        out
    }

    pub fn clip_actions(&self) -> Vec<ClipAction> {
        let mut out = Vec::new();
        for (_, p) in self.live() {
            for a in &p.actions {
                out.push(ClipAction {
                    plugin_id: p.id.clone(),
                    id: a.id.clone(),
                    label: a.label.clone(),
                    file_manager: a.file_manager,
                });
            }
        }
        out
    }

    /// Run `f` for plugin `pi` with errors contained, then handle what it asked for.
    fn call<R>(
        &mut self,
        pi: usize,
        budget: Duration,
        f: impl FnOnce(&Lua, &Plugin) -> mlua::Result<R>,
    ) -> Option<R> {
        let p = self.plugins.get_mut(pi)?;
        let r = p.guard(budget, &self.shared, f);
        self.flush();
        r
    }

    pub fn run_menu(&mut self, pi: usize, item: usize, ctx: &Ctx) {
        self.call(pi, CALL_BUDGET, |lua, p| {
            let run = p.menu.get(item).map(|m| m.run.clone());
            match run {
                Some(f) => f.call(make_ctx(lua, ctx)?),
                None => Ok(()),
            }
        });
    }

    pub fn run_toolbar(&mut self, pi: usize, item: usize, ctx: &Ctx) {
        self.call(pi, CALL_BUDGET, |lua, p| {
            let run = p.toolbar.get(item).map(|m| m.run.clone());
            match run {
                Some(f) => f.call(make_ctx(lua, ctx)?),
                None => Ok(()),
            }
        });
    }

    /// Run a per-clip action on each of `paths`. Err if there's no such action.
    pub fn run_clip_action(
        &mut self,
        plugin_id: &str,
        action: &str,
        paths: &[PathBuf],
        ctx: &Ctx,
    ) -> Result<(), String> {
        let Some(pi) = self.plugins.iter().position(|p| p.id == plugin_id) else {
            return Err(format!(
                "no plugin named {plugin_id:?} in {}",
                self.dir.display()
            ));
        };
        if let Some(e) = &self.plugins[pi].error {
            return Err(format!("plugin {plugin_id} isn't working: {e}"));
        }
        if !self.plugins[pi].enabled {
            return Err(format!("plugin {plugin_id} is turned off in Settings"));
        }
        let Some(ai) = self.plugins[pi].actions.iter().position(|a| a.id == action) else {
            return Err(format!("plugin {plugin_id} has no action {action:?}"));
        };
        for path in paths {
            self.call(pi, CALL_BUDGET, |lua, p| {
                let f = p.actions[ai].run.clone();
                f.call::<()>((path_string(path), make_ctx(lua, ctx)?))
            });
        }
        match &self.plugins[pi].error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    pub fn clip_opened(&mut self, ctx: &Ctx) {
        for pi in 0..self.plugins.len() {
            if self.plugins[pi].hooks.clip_opened.is_some() {
                self.call(pi, CALL_BUDGET, |lua, p| {
                    let f = p.hooks.clip_opened.clone().unwrap();
                    let c = make_ctx(lua, ctx)?;
                    f.call::<()>((c.get::<Value>("clip")?, c))
                });
            }
        }
    }

    /// Ask every plugin what to add to `job`. Err when one cancels the export.
    pub fn export_start(&mut self, job: &Job) -> Result<Extra, String> {
        let mut extra = Extra::default();
        for pi in 0..self.plugins.len() {
            if self.plugins[pi].hooks.export_start.is_none() {
                continue;
            }
            let r = self.call(pi, CALL_BUDGET, |lua, p| {
                let f = p.hooks.export_start.clone().unwrap();
                let v: Value = f.call(job_table(lua, job)?)?;
                parse_edits(v)
            });
            let Some(edits) = r else { continue };
            if let Some(reason) = edits.cancel {
                return Err(reason);
            }
            let id = &self.plugins[pi].id;
            if edits.vf.is_some() || edits.af.is_some() {
                extra.filter_plugins.push(id.clone());
            }
            extra.vf.extend(edits.vf);
            extra.af.extend(edits.af);
            extra.output_args.extend(edits.output_args);
            if let Some(m) = edits.meta {
                extra.meta.insert(id.clone(), m);
            }
            extra.tags.extend(edits.tags);
        }
        Ok(extra)
    }

    pub fn export_done(&mut self, job: &Job, result: &Result<(PathBuf, u64), String>) {
        for pi in 0..self.plugins.len() {
            if self.plugins[pi].hooks.export_done.is_none() {
                continue;
            }
            self.call(pi, CALL_BUDGET, |lua, p| {
                let f = p.hooks.export_done.clone().unwrap();
                let t = lua.create_table()?;
                t.set("ok", result.is_ok())?;
                t.set("source", path_string(&job.src))?;
                t.set("job", job_table(lua, job)?)?;
                match result {
                    Ok((path, size)) => {
                        t.set("path", path_string(path))?;
                        t.set("size", *size)?;
                    }
                    Err(e) => t.set("error", e.as_str())?,
                }
                f.call::<()>(t)
            });
        }
    }

    pub fn browser_refresh(&mut self, clips: &[PathBuf]) {
        for pi in 0..self.plugins.len() {
            if self.plugins[pi].hooks.browser_refresh.is_none() {
                continue;
            }
            self.call(pi, CALL_BUDGET, |lua, p| {
                let f = p.hooks.browser_refresh.clone().unwrap();
                let list = lua.create_sequence_from(clips.iter().map(|c| path_string(c)))?;
                f.call::<()>(list)
            });
        }
    }

    /// Short texts plugins show on a clip's card in the browser.
    pub fn badges(&mut self, clip: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for pi in 0..self.plugins.len() {
            if self.plugins[pi].hooks.badge.is_none() {
                continue;
            }
            let r = self.call(pi, DRAW_BUDGET, |_, p| {
                let f = p.hooks.badge.clone().unwrap();
                f.call::<Option<String>>(path_string(clip))
            });
            out.extend(r.flatten().filter(|s| !s.trim().is_empty()));
        }
        out
    }

    pub fn has_badges(&self) -> bool {
        self.live().any(|(_, p)| p.hooks.badge.is_some())
    }

    /// Draw the plugin windows that are open.
    pub fn draw_windows(&mut self, ectx: &egui::Context, ctx: &Ctx) {
        for pi in 0..self.plugins.len() {
            for wi in 0..self.plugins[pi].windows.len() {
                let p = &mut self.plugins[pi];
                let w = &p.windows[wi];
                if !w.open || p.error.is_some() {
                    continue;
                }
                let mut open = true;
                let mut win = egui::Window::new(w.title.as_str())
                    .id(egui::Id::new((
                        "clipman-plugin",
                        p.id.as_str(),
                        w.id.as_str(),
                    )))
                    .open(&mut open)
                    .collapsible(false);
                if let Some(width) = w.width {
                    win = win.default_width(width);
                }
                if let Some(h) = w.height {
                    win = win.default_height(h);
                }
                let draw = w.draw.clone();
                p.guard(DRAW_BUDGET, &self.shared, |lua, _| {
                    let c = make_ctx(lua, ctx)?;
                    let mut r = Ok(());
                    win.show(ectx, |ui| {
                        r = super::ui::with_ui(lua, ui, |u| draw.call::<()>((u, c)));
                    });
                    r
                });
                let failed = p.error.is_some();
                p.windows[wi].open = open && !failed;
            }
        }
        self.flush();
    }

    pub fn has_open_windows(&self) -> bool {
        self.live().any(|(_, p)| p.windows.iter().any(|w| w.open))
    }

    /// Processes started with `clipman.spawn` are still running.
    pub fn busy(&self) -> bool {
        !self.running.is_empty()
    }

    /// Call `on_exit` for processes that finished. Returns whether any are still running.
    pub fn poll(&mut self) -> bool {
        let mut i = 0;
        while i < self.running.len() {
            let res = match self.running[i].rx.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => ProcResult {
                    code: None,
                    stdout: String::new(),
                    stderr: "the process thread stopped".into(),
                },
            };
            let r = self.running.remove(i);
            let Some(f) = r.on_exit else { continue };
            if let Some(pi) = self.plugins.iter().position(|p| p.id == r.plugin) {
                self.call(pi, CALL_BUDGET, |_, _| {
                    f.call::<()>((res.code, res.stdout, res.stderr))
                });
            }
        }
        self.busy()
    }

    /// Wait for spawned processes (command-line renders, before exiting).
    pub fn wait_idle(&mut self) {
        let mut said = false;
        while self.poll() {
            if !said {
                eprintln!("clipman: waiting for plugin processes to finish…");
                said = true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn take_clip_cmds(&mut self) -> Vec<ClipCmd> {
        std::mem::take(&mut self.shared.borrow_mut().clip_cmds)
    }

    pub fn has_clip_cmds(&self) -> bool {
        !self.shared.borrow().clip_cmds.is_empty()
    }

    /// Messages for the status line (`clipman.notify`, plugins that stopped).
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.shared.borrow_mut().notices)
    }

    /// Handle requests made during the last call: windows to open / close, processes
    /// to start.
    fn flush(&mut self) {
        let (wins, spawns) = {
            let mut sh = self.shared.borrow_mut();
            (
                std::mem::take(&mut sh.windows),
                std::mem::take(&mut sh.spawns),
            )
        };
        for (pid, wid, open) in wins {
            let p = self.plugins.iter_mut().find(|p| p.id == pid);
            match p.and_then(|p| p.windows.iter_mut().find(|w| w.id == wid)) {
                Some(w) => w.open = open,
                None => self
                    .shared
                    .borrow_mut()
                    .notices
                    .push(format!("{pid}: no window {wid:?}")),
            }
        }
        for s in spawns {
            self.running.push(spawn(s));
        }
    }
}

fn spawn(req: SpawnReq) -> Running {
    let (tx, rx) = mpsc::channel();
    let SpawnReq {
        plugin,
        args,
        cwd,
        stdin,
        on_exit,
    } = req;
    std::thread::spawn(move || {
        use std::process::Stdio;
        let mut cmd = crate::media::quiet_command(&args[0]);
        cmd.args(&args[1..])
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        let res = match cmd.spawn() {
            Ok(mut child) => {
                if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
                    use std::io::Write;
                    let _ = pipe.write_all(text.as_bytes());
                }
                match child.wait_with_output() {
                    Ok(o) => ProcResult {
                        code: o.status.code(),
                        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
                    },
                    Err(e) => ProcResult {
                        code: None,
                        stdout: String::new(),
                        stderr: e.to_string(),
                    },
                }
            }
            Err(e) => ProcResult {
                code: None,
                stdout: String::new(),
                stderr: format!("couldn't start {}: {e}", args[0]),
            },
        };
        let _ = tx.send(res);
    });
    Running {
        plugin,
        rx,
        on_exit,
    }
}

impl Plugin {
    fn new(id: String, dir: PathBuf, enabled: bool) -> Self {
        Self {
            name: id.clone(),
            id,
            dir,
            version: String::new(),
            description: String::new(),
            enabled,
            lua: None,
            error: None,
            deadline: Default::default(),
            menu: Vec::new(),
            menus: Vec::new(),
            toolbar: Vec::new(),
            windows: Vec::new(),
            settings: Vec::new(),
            actions: Vec::new(),
            hooks: Hooks::default(),
        }
    }

    fn item(&self, pi: usize, i: usize, screen: Screen) -> Option<MenuItem> {
        let m = &self.menu[i];
        m.screen.is_none_or(|s| s == screen).then(|| MenuItem {
            plugin: pi,
            item: i,
            label: m.label.clone(),
            shortcut: m.shortcut,
        })
    }

    /// Menu entries visible on `screen`: hidden items and empty submenus left out, and no
    /// separators at either end or twice in a row.
    fn entries(&self, pi: usize, entries: &[Entry], screen: Screen) -> Vec<MenuEntry> {
        let mut out = Vec::new();
        for e in entries {
            let e = match e {
                Entry::Item(i) => self.item(pi, *i, screen).map(MenuEntry::Item),
                Entry::Separator => Some(MenuEntry::Separator),
                Entry::Sub(label, sub) => {
                    let sub = self.entries(pi, sub, screen);
                    (!sub.is_empty()).then(|| MenuEntry::Submenu(label.clone(), sub))
                }
            };
            match e {
                Some(MenuEntry::Separator)
                    if out.is_empty() || matches!(out.last(), Some(MenuEntry::Separator)) => {}
                Some(e) => out.push(e),
                None => {}
            }
        }
        if matches!(out.last(), Some(MenuEntry::Separator)) {
            out.pop();
        }
        out
    }

    /// Read a menu item table into `self.menu`; returns its index.
    fn read_item(
        &mut self,
        m: &Table,
        w: &str,
        inherited: Option<Screen>,
    ) -> anyhow::Result<usize> {
        let shortcut = match get::<Option<String>>(m, w, "shortcut")? {
            Some(s) => {
                let sc =
                    parse_shortcut(&s).ok_or_else(|| anyhow!("{w}.shortcut: can't read {s:?}"))?;
                let name = sc.logical_key.name();
                let f_key = name.len() > 1
                    && name.starts_with('F')
                    && name[1..].chars().all(|c| c.is_ascii_digit());
                if !(sc.modifiers.command || sc.modifiers.alt || f_key) {
                    bail!(
                        "{w}.shortcut: {s:?} needs Ctrl or Alt (plain keys belong to the \
                         editor), or use F1\u{2013}F35"
                    );
                }
                Some(sc)
            }
            None => None,
        };
        self.menu.push(MenuDef {
            label: get(m, w, "label")?,
            shortcut,
            screen: read_screen(m, w)?.unwrap_or(inherited),
            run: get(m, w, "run")?,
        });
        Ok(self.menu.len() - 1)
    }

    /// Read a dropdown's `items`: actions, `{ separator = true }`, or submenus
    /// (`{ label = …, items = { … } }`).
    fn read_entries(
        &mut self,
        items: Vec<Table>,
        w: &str,
        screen: Option<Screen>,
        depth: usize,
    ) -> anyhow::Result<Vec<Entry>> {
        let mut out = Vec::new();
        for (i, m) in items.into_iter().enumerate() {
            let w = format!("{w}.items[{}]", i + 1);
            if get::<Option<bool>>(&m, &w, "separator")? == Some(true) {
                out.push(Entry::Separator);
            } else if let Some(sub) = get::<Option<Vec<Table>>>(&m, &w, "items")? {
                if depth >= MAX_MENU_DEPTH {
                    bail!("{w}: submenus nest at most {MAX_MENU_DEPTH} deep");
                }
                let screen = read_screen(&m, &w)?.unwrap_or(screen);
                let label: String = get(&m, &w, "label")?;
                out.push(Entry::Sub(label, self.read_entries(sub, &w, screen, depth + 1)?));
            } else {
                out.push(Entry::Item(self.read_item(&m, &w, screen)?));
            }
        }
        Ok(out)
    }

    /// Run a callback with a time limit. An error (or running too long) stops the
    /// plugin for the rest of the session.
    fn guard<R>(
        &mut self,
        budget: Duration,
        shared: &Rc<RefCell<Shared>>,
        f: impl FnOnce(&Lua, &Plugin) -> mlua::Result<R>,
    ) -> Option<R> {
        if self.error.is_some() {
            return None;
        }
        let lua = self.lua.as_ref()?;
        self.deadline.set(Some(Instant::now() + budget));
        let r = f(lua, self);
        self.deadline.set(None);
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                let msg = error_text(&e);
                eprintln!("clipman: plugin {} stopped: {msg}", self.id);
                shared
                    .borrow_mut()
                    .notices
                    .push(format!("Plugin {} stopped: {msg}", self.name));
                self.error = Some(msg);
                // its windows go away with it
                for w in &mut self.windows {
                    w.open = false;
                }
                None
            }
        }
    }

    fn load(
        &mut self,
        shared: &Rc<RefCell<Shared>>,
        cfg: Option<&PluginCfg>,
    ) -> anyhow::Result<()> {
        let lua = Lua::new();
        let deadline = self.deadline.clone();
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(10_000),
            move |_, _| {
                if deadline.get().is_some_and(|d| Instant::now() > d) {
                    Err(mlua::Error::runtime(
                        "took too long (plugin code must not block the window: use \
                         clipman.spawn for slow work)",
                    ))
                } else {
                    Ok(VmState::Continue)
                }
            },
        )
        .map_err(lua_err)?;
        // `require` finds modules next to init.lua
        let package: Table = lua.globals().get("package").map_err(lua_err)?;
        let path: String = package.get("path").map_err(lua_err)?;
        let d = self.dir.to_string_lossy();
        let sep = std::path::MAIN_SEPARATOR;
        package
            .set(
                "path",
                format!("{d}{sep}?.lua;{d}{sep}?{sep}init.lua;{path}"),
            )
            .map_err(lua_err)?;
        install_api(&lua, &self.id, &self.dir, shared.clone()).map_err(lua_err)?;
        super::ui::register(&lua).map_err(lua_err)?;

        let init = self.dir.join("init.lua");
        let code =
            std::fs::read(&init).with_context(|| format!("couldn't read {}", init.display()))?;
        self.deadline.set(Some(Instant::now() + LOAD_BUDGET));
        let spec = lua
            .load(code)
            .set_name(format!("@{}", init.display()))
            .eval::<Table>();
        self.deadline.set(None);
        let spec = spec.map_err(|e| anyhow!("{}", error_text(&e)))?;
        self.read_spec(&spec)?;
        set_settings_table(&lua, &self.settings, cfg).map_err(lua_err)?;
        self.lua = Some(lua);
        Ok(())
    }

    /// Read what init.lua returned.
    fn read_spec(&mut self, t: &Table) -> anyhow::Result<()> {
        let api: Option<i64> = get(t, "api", "api")?;
        if let Some(v) = api
            && v > API_VERSION
        {
            bail!("needs plugin API {v}; this clipman has {API_VERSION}. Update clipman.");
        }
        if let Some(n) = get::<Option<String>>(t, "name", "name")? {
            self.name = n;
        }
        self.version = get::<Option<String>>(t, "version", "version")?.unwrap_or_default();
        self.description =
            get::<Option<String>>(t, "description", "description")?.unwrap_or_default();

        for (i, m) in list(t, "menus")?.into_iter().enumerate() {
            let w = format!("menus[{}]", i + 1);
            let title: String = get(&m, &w, "title")?;
            if title.trim().is_empty() {
                bail!("{w}.title is empty");
            }
            if RESERVED_MENUS.iter().any(|r| r.eq_ignore_ascii_case(title.trim())) {
                bail!("{w}.title: {title:?} is clipman's own menu");
            }
            let screen = read_screen(&m, &w)?.flatten();
            let items: Vec<Table> = get(&m, &w, "items")?;
            let entries = self.read_entries(items, &w, screen, 1)?;
            self.menus.push(MenuTree { title, entries });
        }
        for (i, b) in list(t, "toolbar")?.into_iter().enumerate() {
            let w = format!("toolbar[{}]", i + 1);
            self.toolbar.push(ToolDef {
                label: get(&b, &w, "label")?,
                tooltip: get::<Option<String>>(&b, &w, "tooltip")?.unwrap_or_default(),
                run: get(&b, &w, "run")?,
            });
        }
        for (i, b) in list(t, "windows")?.into_iter().enumerate() {
            let w = format!("windows[{}]", i + 1);
            let id: String = get(&b, &w, "id")?;
            self.windows.push(WindowDef {
                title: get::<Option<String>>(&b, &w, "title")?.unwrap_or_else(|| id.clone()),
                id,
                width: get(&b, &w, "width")?,
                height: get(&b, &w, "height")?,
                draw: get(&b, &w, "draw")?,
                open: false,
            });
        }
        for (i, b) in list(t, "settings")?.into_iter().enumerate() {
            let w = format!("settings[{}]", i + 1);
            let key: String = get(&b, &w, "key")?;
            let kind = match get::<Option<String>>(&b, &w, "type")?.as_deref() {
                None | Some("string") => SettingKind::String,
                Some("bool") => SettingKind::Bool,
                Some("number") => SettingKind::Number {
                    min: get(&b, &w, "min")?,
                    max: get(&b, &w, "max")?,
                },
                Some("choice") => {
                    let opts: Vec<String> = get(&b, &w, "options")?;
                    if opts.is_empty() {
                        bail!("{w}.options: a choice needs options");
                    }
                    SettingKind::Choice(opts)
                }
                Some("folder") => SettingKind::Folder,
                Some(s) => bail!("{w}.type: {s:?} isn't string, bool, number, choice or folder"),
            };
            let lua_default: Value = b.get("default").map_err(lua_err)?;
            let mut default = if lua_default.is_nil() {
                serde_json::Value::Null
            } else {
                json_of(lua_default).map_err(lua_err)?
            };
            if default.is_null() {
                default = match &kind {
                    SettingKind::String | SettingKind::Folder => "".into(),
                    SettingKind::Bool => false.into(),
                    SettingKind::Number { min, .. } => min.unwrap_or(0.0).into(),
                    SettingKind::Choice(o) => o[0].clone().into(),
                };
            }
            self.settings.push(SettingDef {
                label: get::<Option<String>>(&b, &w, "label")?.unwrap_or_else(|| key.clone()),
                key,
                kind,
                default,
                help: get::<Option<String>>(&b, &w, "help")?.unwrap_or_default(),
            });
        }
        for (i, b) in list(t, "clip_actions")?.into_iter().enumerate() {
            let w = format!("clip_actions[{}]", i + 1);
            let id: String = get(&b, &w, "id")?;
            if !valid_id(&id) {
                bail!("{w}.id: use lowercase letters, digits, - and _ only");
            }
            self.actions.push(ActionDef {
                id,
                label: get(&b, &w, "label")?,
                file_manager: get::<Option<bool>>(&b, &w, "file_manager")?.unwrap_or(false),
                run: get(&b, &w, "run")?,
            });
        }
        self.hooks = Hooks {
            clip_opened: get(t, "on_clip_opened", "on_clip_opened")?,
            export_start: get(t, "on_export_start", "on_export_start")?,
            export_done: get(t, "on_export_done", "on_export_done")?,
            browser_refresh: get(t, "on_browser_refresh", "on_browser_refresh")?,
            badge: get(t, "badge", "badge")?,
        };
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A table's `screen` field: None when missing, Some(None) for "any".
fn read_screen(t: &Table, w: &str) -> anyhow::Result<Option<Option<Screen>>> {
    Ok(match get::<Option<String>>(t, w, "screen")?.as_deref() {
        None => None,
        Some("any") => Some(None),
        Some("editor") => Some(Some(Screen::Editor)),
        Some("browser") => Some(Some(Screen::Browser)),
        Some(s) => bail!("{w}.screen: {s:?} isn't \"editor\", \"browser\" or \"any\""),
    })
}

fn lua_err(e: mlua::Error) -> anyhow::Error {
    anyhow!("{}", error_text(&e))
}

/// A readable one-paragraph error (mlua nests callback errors with tracebacks).
fn error_text(e: &mlua::Error) -> String {
    let mut e = e;
    while let mlua::Error::CallbackError { cause, .. } = e {
        e = cause;
    }
    let s = e.to_string();
    let s = s.split("\nstack traceback:").next().unwrap_or(&s).trim();
    if s.chars().count() > 600 {
        format!("{}…", s.chars().take(600).collect::<String>())
    } else {
        s.to_string()
    }
}

/// A field of a spec table, with the place in the error.
fn get<T: FromLua>(t: &Table, what: &str, key: &str) -> anyhow::Result<T> {
    t.get::<T>(key).map_err(|e| {
        let what = if what == key {
            key.to_string()
        } else {
            format!("{what}.{key}")
        };
        anyhow!("{what}: {}", error_text(&e))
    })
}

/// A list of tables (`menus = { {...}, {...} }`), empty when the field is missing.
fn list(t: &Table, key: &str) -> anyhow::Result<Vec<Table>> {
    get::<Option<Vec<Table>>>(t, key, key).map(Option::unwrap_or_default)
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn ser_opts() -> SerializeOptions {
    SerializeOptions::new()
        .serialize_none_to_null(false)
        .serialize_unit_to_null(false)
}

fn de_opts() -> DeserializeOptions {
    DeserializeOptions::new()
        .deny_unsupported_types(false)
        .deny_recursive_tables(true)
        .sort_keys(true)
}

fn from_json(lua: &Lua, v: &serde_json::Value) -> mlua::Result<Value> {
    lua.to_value_with(v, ser_opts())
}

/// A Lua value as JSON (functions and other non-data values are left out).
fn json_of(v: Value) -> mlua::Result<serde_json::Value> {
    use serde::Deserialize;
    serde_json::Value::deserialize(mlua::serde::Deserializer::new_with_options(v, de_opts()))
}

/// `clipman.settings`: the plugin's setting values, with defaults filled in.
fn set_settings_table(lua: &Lua, defs: &[SettingDef], cfg: Option<&PluginCfg>) -> mlua::Result<()> {
    let t = lua.create_table()?;
    for d in defs {
        let v = cfg
            .and_then(|c| c.values.get(&d.key))
            .filter(|v| !v.is_null())
            .unwrap_or(&d.default);
        t.set(d.key.as_str(), from_json(lua, v)?)?;
    }
    let cm: Table = lua.globals().get("clipman")?;
    cm.set("settings", t)
}

fn parse_color(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([b(0)?, b(2)?, b(4)?])
}

pub(super) fn color_of(s: &str) -> Option<egui::Color32> {
    parse_color(s).map(|[r, g, b]| egui::Color32::from_rgb(r, g, b))
}

const CLIP_METHODS: &str = "clipman.clip_methods";

/// `ctx` for callbacks: `{ screen = "editor", clip = {...} | nil }`.
fn make_ctx(lua: &Lua, ctx: &Ctx) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("screen", ctx.screen.name())?;
    if let Some(c) = &ctx.clip {
        t.set("clip", clip_table(lua, c)?)?;
    }
    Ok(t)
}

/// The editor's clip: a snapshot of its state, plus methods that change it
/// (applied by the editor after the callback returns).
fn clip_table(lua: &Lua, c: &ClipState) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("path", path_string(&c.path))?;
    t.set(
        "name",
        c.path.file_name().map(|n| n.to_string_lossy().into_owned()),
    )?;
    t.set("duration", c.duration)?;
    t.set("fps", c.fps)?;
    t.set("width", c.width)?;
    t.set("height", c.height)?;
    let names = lua.create_table()?;
    for (i, n) in c.audio_names.iter().enumerate() {
        names.set(
            i + 1,
            n.clone().unwrap_or_else(|| format!("Track {}", i + 1)),
        )?;
    }
    t.set("audio_names", names)?;
    t.set("start", c.start)?;
    t.set("end", c.end)?;
    t.set("playhead", c.playhead)?;
    let mt = lua.create_table()?;
    mt.set("__index", lua.named_registry_value::<Table>(CLIP_METHODS)?)?;
    t.set_metatable(Some(mt))?;
    Ok(t)
}

fn job_table(lua: &Lua, job: &Job) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("src", path_string(&job.src))?;
    t.set("dst", path_string(&job.dst))?;
    t.set(
        "kind",
        if job.preset.is_original() {
            "original"
        } else {
            "compress"
        },
    )?;
    let preset = serde_json::to_value(&job.preset).map_err(mlua::Error::external)?;
    t.set("preset", from_json(lua, &preset)?)?;
    t.set("start", job.start)?;
    t.set("end", job.end)?;
    // 1-based, as in --audio
    t.set(
        "tracks",
        job.tracks.iter().map(|i| i + 1).collect::<Vec<_>>(),
    )?;
    t.set(
        "denoise",
        job.denoise.iter().map(|i| i + 1).collect::<Vec<_>>(),
    )?;
    Ok(t)
}

#[derive(Default)]
struct Edits {
    cancel: Option<String>,
    vf: Option<String>,
    af: Option<String>,
    output_args: Vec<String>,
    meta: Option<serde_json::Value>,
    tags: Vec<(String, String)>,
}

/// What `on_export_start` returned (nil = nothing to add).
fn parse_edits(v: Value) -> mlua::Result<Edits> {
    let Value::Table(t) = v else {
        if v.is_nil() {
            return Ok(Edits::default());
        }
        return Err(mlua::Error::runtime(
            "on_export_start must return a table or nil",
        ));
    };
    let nonempty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    let mut tags: Vec<(String, String)> = t
        .get::<Option<std::collections::HashMap<String, String>>>("tags")?
        .unwrap_or_default()
        .into_iter()
        .collect();
    tags.sort();
    if let Some((k, _)) = tags
        .iter()
        .find(|(k, _)| crate::render::RESERVED_TAGS.contains(&k.as_str()))
    {
        return Err(mlua::Error::runtime(format!(
            "tags: {k:?} is clipman's own; put plugin data in `meta` instead"
        )));
    }
    let meta: Value = t.get("meta")?;
    Ok(Edits {
        cancel: t.get("cancel")?,
        vf: nonempty(t.get("vf")?),
        af: nonempty(t.get("af")?),
        output_args: t
            .get::<Option<Vec<String>>>("output_args")?
            .unwrap_or_default(),
        meta: if meta.is_nil() {
            None
        } else {
            Some(json_of(meta)?)
        },
        tags,
    })
}

// ---------------------------------------------------------------------------
// The `clipman` table
// ---------------------------------------------------------------------------

fn install_api(lua: &Lua, id: &str, dir: &Path, shared: Rc<RefCell<Shared>>) -> mlua::Result<()> {
    let cm = lua.create_table()?;
    cm.set("version", env!("CARGO_PKG_VERSION"))?;
    cm.set("api", API_VERSION)?;
    cm.set("plugin_id", id)?;
    cm.set("plugin_dir", path_string(dir))?;
    cm.set("os", std::env::consts::OS)?;
    cm.set("settings", lua.create_table()?)?;

    let pid = id.to_string();
    cm.set(
        "log",
        lua.create_function(move |_, args: mlua::Variadic<Value>| {
            let parts: mlua::Result<Vec<String>> = args.iter().map(Value::to_string).collect();
            eprintln!("[{pid}] {}", parts?.join(" "));
            Ok(())
        })?,
    )?;

    let (sh, pid) = (shared.clone(), id.to_string());
    cm.set(
        "notify",
        lua.create_function(move |_, text: String| {
            sh.borrow_mut().notices.push(format!("{pid}: {text}"));
            Ok(())
        })?,
    )?;

    let (sh, pid) = (shared.clone(), id.to_string());
    cm.set(
        "spawn",
        lua.create_function(move |_, opts: Table| {
            let args: Vec<String> = opts.get("args")?;
            if args.is_empty() {
                return Err(mlua::Error::runtime("spawn: args is empty"));
            }
            sh.borrow_mut().spawns.push(SpawnReq {
                plugin: pid.clone(),
                args,
                cwd: opts.get::<Option<String>>("cwd")?.map(PathBuf::from),
                stdin: opts.get("stdin")?,
                on_exit: opts.get("on_exit")?,
            });
            Ok(())
        })?,
    )?;

    cm.set(
        "filter_escape",
        lua.create_function(|_, s: String| Ok(filter_escape(&s)))?,
    )?;

    cm.set(
        "open",
        lua.create_function(|_, target: String| {
            crate::ui::open_path(Path::new(&target));
            Ok(())
        })?,
    )?;

    for (name, open) in [("open_window", true), ("close_window", false)] {
        let (sh, pid) = (shared.clone(), id.to_string());
        cm.set(
            name,
            lua.create_function(move |_, wid: String| {
                let mut s = sh.borrow_mut();
                if open && !s.gui {
                    eprintln!("[{pid}] open_window({wid:?}): no windows in a command-line render");
                    return Ok(());
                }
                s.windows.push((pid.clone(), wid, open));
                Ok(())
            })?,
        )?;
    }

    let sh = shared.clone();
    cm.set(
        "clips_folder",
        lua.create_function(move |_, ()| Ok(path_string(&sh.borrow().clips_folder)))?,
    )?;
    let sh = shared.clone();
    cm.set(
        "export_folder",
        lua.create_function(move |_, ()| Ok(path_string(&sh.borrow().export_folder)))?,
    )?;

    // metadata: sidecars for source clips, the `clipman` tag of exports
    let meta = lua.create_table()?;
    let pid = id.to_string();
    meta.set(
        "get",
        lua.create_function(move |lua, path: String| {
            match super::meta::read(Path::new(&path), &pid) {
                Some(v) => from_json(lua, &v),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    meta.set(
        "get_all",
        lua.create_function(|lua, path: String| {
            from_json(
                lua,
                &serde_json::Value::Object(super::meta::read_all(Path::new(&path))),
            )
        })?,
    )?;
    let pid = id.to_string();
    meta.set(
        "set",
        lua.create_function(move |_, (path, v): (String, Value)| {
            let v = if v.is_nil() { None } else { Some(json_of(v)?) };
            super::meta::write(Path::new(&path), &pid, v).map_err(mlua::Error::external)
        })?,
    )?;
    meta.set(
        "read_export",
        lua.create_function(|lua, path: String| {
            match crate::media::container_tag(Path::new(&path), "clipman")
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            {
                Some(v) => from_json(lua, &v),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    cm.set("meta", meta)?;

    let json = lua.create_table()?;
    json.set(
        "encode",
        lua.create_function(|_, (v, pretty): (Value, Option<bool>)| {
            let j = json_of(v)?;
            let s = if pretty == Some(true) {
                serde_json::to_string_pretty(&j)
            } else {
                serde_json::to_string(&j)
            };
            s.map_err(mlua::Error::external)
        })?,
    )?;
    json.set(
        "decode",
        lua.create_function(|lua, s: String| {
            let v: serde_json::Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
            from_json(lua, &v)
        })?,
    )?;
    cm.set("json", json)?;

    lua.globals().set("clipman", cm)?;

    // methods of the `clip` table in callbacks
    let methods = lua.create_table()?;
    let (sh, pid) = (shared.clone(), id.to_string());
    methods.set(
        "add_marker",
        lua.create_function(move |_, (_this, o): (Table, Table)| {
            let color = match o.get::<Option<String>>("color")? {
                Some(c) => Some(parse_color(&c).ok_or_else(|| {
                    mlua::Error::runtime(format!("color {c:?}: use \"#rrggbb\""))
                })?),
                None => None,
            };
            sh.borrow_mut().clip_cmds.push(ClipCmd::AddMarker(Marker {
                plugin: pid.clone(),
                t: o.get("t")?,
                label: o.get::<Option<String>>("label")?.unwrap_or_default(),
                color,
            }));
            Ok(())
        })?,
    )?;
    let (sh, pid) = (shared.clone(), id.to_string());
    methods.set(
        "clear_markers",
        lua.create_function(move |_, _this: Table| {
            sh.borrow_mut()
                .clip_cmds
                .push(ClipCmd::ClearMarkers(pid.clone()));
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    methods.set(
        "set_range",
        lua.create_function(move |_, (_this, a, b): (Table, f64, f64)| {
            sh.borrow_mut().clip_cmds.push(ClipCmd::SetRange(a, b));
            Ok(())
        })?,
    )?;
    let sh = shared;
    methods.set(
        "seek",
        lua.create_function(move |_, (_this, t): (Table, f64)| {
            sh.borrow_mut().clip_cmds.push(ClipCmd::Seek(t));
            Ok(())
        })?,
    )?;
    lua.set_named_registry_value(CLIP_METHODS, methods)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugins folder with one plugin `id` whose init.lua is `code`.
    fn host_with(id: &str, code: &str) -> (Host, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "clipman-plugins-{}-{id}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(id)).unwrap();
        std::fs::write(dir.join(id).join("init.lua"), code).unwrap();
        let h = Host::load_from(dir.clone(), &Settings::default());
        (h, dir)
    }

    fn job() -> Job {
        Job {
            src: "/clips/Replay.mp4".into(),
            dst: "/out/Replay_720p.mp4".into(),
            preset: crate::settings::default_presets().remove(0),
            start: Some(1.0),
            end: Some(4.0),
            tracks: vec![0, 1],
            denoise: vec![],
            strength: 100,
            encoder: crate::settings::Encoder::Cpu,
            extra: Extra::default(),
        }
    }

    #[test]
    fn registers_and_runs() {
        let (mut h, dir) = host_with(
            "demo",
            r#"
            local count = 0
            return {
              name = "Demo", version = "1.0", api = 1,
              settings = { { key = "game", label = "Game", default = "Apex" } },
              menus = { { title = "Demo", screen = "editor", items = {
                { label = "Mark", shortcut = "Ctrl+Shift+M",
                  run = function(ctx)
                    count = count + 1
                    ctx.clip:add_marker{ t = ctx.clip.playhead, label = clipman.settings.game }
                    ctx.clip:set_range(1, 2)
                  end },
              } } },
              toolbar = { { label = "Hi", run = function() clipman.notify("hi " .. count) end } },
              clip_actions = { { id = "tag", label = "Tag", file_manager = true,
                                 run = function(path) clipman.notify(path) end } },
            }
            "#,
        );
        let info = &h.infos()[0];
        assert_eq!((info.name.as_str(), info.error.as_deref()), ("Demo", None));
        assert!(h.menus(Screen::Browser).is_empty());
        let items = h.shortcut_items(Screen::Editor);
        assert_eq!(items[0].label, "Mark");
        assert!(items[0].shortcut.is_some());
        let ctx = Ctx {
            screen: Screen::Editor,
            clip: Some(ClipState {
                playhead: 3.5,
                ..Default::default()
            }),
        };
        h.run_menu(0, 0, &ctx);
        assert_eq!(
            h.take_clip_cmds(),
            [
                ClipCmd::AddMarker(Marker {
                    plugin: "demo".into(),
                    t: 3.5,
                    label: "Apex".into(),
                    color: None
                }),
                ClipCmd::SetRange(1.0, 2.0)
            ]
        );
        h.run_toolbar(0, 0, &ctx);
        assert_eq!(h.take_notices(), ["demo: hi 1"]);
        assert_eq!(h.clip_actions()[0].arg(), "demo:tag");
        h.run_clip_action("demo", "tag", &["/a.mp4".into()], &ctx)
            .unwrap();
        assert_eq!(h.take_notices(), ["demo: /a.mp4"]);
        assert!(h.run_clip_action("demo", "nope", &[], &ctx).is_err());

        // new setting values reach the plugin without a reload
        let mut s = Settings::default();
        s.plugins.insert(
            "demo".into(),
            PluginCfg {
                enabled: true,
                values: [("game".to_string(), "Quake".into())].into_iter().collect(),
            },
        );
        h.apply_settings(&s);
        h.run_menu(0, 0, &ctx);
        assert!(matches!(&h.take_clip_cmds()[0], ClipCmd::AddMarker(m) if m.label == "Quake"));
        // turning it off unloads it
        s.plugins.get_mut("demo").unwrap().enabled = false;
        h.apply_settings(&s);
        assert!(h.menus(Screen::Editor).is_empty());
        assert!(!h.infos()[0].enabled);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn own_menus() {
        let (mut h, dir) = host_with(
            "up",
            r#"
            local function f() clipman.notify("ran") end
            return {
              menus = {
                { title = "Upload", items = {
                    { separator = true },
                    { label = "Upload clip", shortcut = "Ctrl+U", screen = "editor", run = f },
                    { separator = true },
                    { separator = true },
                    { label = "Services", items = {
                        { label = "Streamable", run = f },
                        { label = "Editor only", screen = "editor", run = f },
                    } },
                    { label = "Empty here", screen = "editor", items = {
                        { label = "x", run = f },
                    } },
                    { separator = true },
                } },
                { title = "Upload", screen = "browser", items = { { label = "Second", run = f } } },
              },
            }
            "#,
        );
        assert_eq!(h.infos()[0].error, None);
        let show = |es: &[MenuEntry]| -> Vec<String> {
            es.iter()
                .map(|e| match e {
                    MenuEntry::Item(i) => i.label.clone(),
                    MenuEntry::Separator => "--".into(),
                    MenuEntry::Submenu(l, sub) => {
                        let inner: Vec<_> = sub
                            .iter()
                            .map(|e| match e {
                                MenuEntry::Item(i) => i.label.clone(),
                                _ => "?".into(),
                            })
                            .collect();
                        format!("{l}>{}", inner.join(","))
                    }
                })
                .collect()
        };
        let ed = h.menus(Screen::Editor);
        assert_eq!(ed.len(), 1);
        assert_eq!(ed[0].title, "Upload");
        assert_eq!(
            show(&ed[0].entries),
            ["Upload clip", "--", "Services>Streamable,Editor only", "Empty here>x"]
        );
        // the same title from another table is merged; editor-only items and empty
        // submenus disappear in the browser
        let br = h.menus(Screen::Browser);
        assert_eq!(show(&br[0].entries), ["Services>Streamable", "--", "Second"]);
        let sc = h.shortcut_items(Screen::Editor);
        assert_eq!(sc[0].label, "Upload clip");
        assert!(h.shortcut_items(Screen::Browser).is_empty());
        let MenuEntry::Item(it) = &ed[0].entries[0] else { panic!() };
        h.run_menu(it.plugin, it.item, &Ctx::default());
        assert_eq!(h.take_notices(), ["up: ran"]);
        std::fs::remove_dir_all(dir).unwrap();

        let (h, dir) = host_with(
            "clash",
            "return { menus = { { title = 'help', items = {} } } }",
        );
        assert!(h.infos()[0].error.as_ref().unwrap().contains("clipman's own menu"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn errors_stop_the_plugin() {
        let (mut h, dir) = host_with(
            "bad",
            r#"return { menus = { { title = "T", items = {
                 { label = "Boom", run = function() error("kaput") end } } } } }"#,
        );
        h.run_menu(0, 0, &Ctx::default());
        let e = h.infos()[0].error.clone().unwrap();
        assert!(e.contains("kaput"), "{e}");
        assert!(h.take_notices()[0].contains("kaput"));
        assert!(h.menus(Screen::Browser).is_empty()); // gone for the session
        std::fs::remove_dir_all(dir).unwrap();

        let (h, dir) = host_with(
            "broken",
            "return { menus = { { title = 'T', items = { { label = 'No run' } } } } }",
        );
        let e = h.infos()[0].error.clone().unwrap();
        assert!(e.starts_with("menus[1].items[1].run:"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();

        let (h, dir) = host_with(
            "plainkey",
            "return { menus = { { title = 'T', items = {
               { label = 'x', shortcut = 'Shift+I', run = print } } } } }",
        );
        assert!(
            h.infos()[0]
                .error
                .as_ref()
                .unwrap()
                .contains("needs Ctrl or Alt")
        );
        std::fs::remove_dir_all(dir).unwrap();
        let (h, dir) = host_with(
            "fkey",
            "return { menus = { { title = 'T', items = {
               { label = 'x', shortcut = 'F5', run = print } } } } }",
        );
        assert_eq!(h.infos()[0].error, None);
        std::fs::remove_dir_all(dir).unwrap();

        let (h, dir) = host_with("future", "return { api = 99 }");
        assert!(h.infos()[0].error.as_ref().unwrap().contains("API 99"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runaway_code_is_stopped() {
        let (mut h, dir) = host_with(
            "loop",
            r#"return { menus = { { title = "T", items = {
                 { label = "Spin", run = function() while true do end end } } } } }"#,
        );
        let t = Instant::now();
        h.run_menu(0, 0, &Ctx::default());
        assert!(t.elapsed() < CALL_BUDGET + Duration::from_secs(1));
        assert!(h.infos()[0].error.as_ref().unwrap().contains("too long"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_edits() {
        let (mut h, dir) = host_with(
            "wm",
            r#"
            return {
              on_export_start = function(job)
                if job.kind ~= "compress" then return nil end
                return {
                  vf = "drawtext=text='hi'",
                  af = "volume=1.5",
                  output_args = { "-x264-params", "aq-mode=3" },
                  meta = { src = job.src, tracks = job.tracks, empty = {} },
                  tags = { title = "My clip" },
                }
              end,
            }
            "#,
        );
        let x = h.export_start(&job()).unwrap();
        assert_eq!(x.vf, ["drawtext=text='hi'"]);
        assert_eq!(x.af, ["volume=1.5"]);
        assert_eq!(x.filter_plugins, ["wm"]);
        assert_eq!(x.output_args, ["-x264-params", "aq-mode=3"]);
        assert_eq!(x.meta["wm"]["src"], "/clips/Replay.mp4");
        assert_eq!(x.meta["wm"]["tracks"], serde_json::json!([1, 2]));
        assert_eq!(x.tags, [("title".to_string(), "My clip".to_string())]);
        let mut orig = job();
        orig.preset.kind = crate::settings::PresetKind::Original;
        assert_eq!(h.export_start(&orig).unwrap(), Extra::default());
        std::fs::remove_dir_all(dir).unwrap();

        let (mut h, dir) = host_with(
            "veto",
            r#"return { on_export_start = function() return { cancel = "not today" } end }"#,
        );
        assert_eq!(h.export_start(&job()).unwrap_err(), "not today");
        std::fs::remove_dir_all(dir).unwrap();

        let (mut h, dir) = host_with(
            "tagger",
            r#"return { on_export_start = function() return { tags = { comment = "x" } } end }"#,
        );
        assert_eq!(h.export_start(&job()).unwrap(), Extra::default());
        assert!(
            h.infos()[0]
                .error
                .as_ref()
                .unwrap()
                .contains("clipman's own")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn examples_load() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins");
        let mut s = Settings::default();
        s.plugins.insert(
            "watermark".into(),
            PluginCfg {
                enabled: true,
                values: [("text".to_string(), "it's 50%: ok".into())]
                    .into_iter()
                    .collect(),
            },
        );
        let mut h = Host::load_from(dir, &s);
        let infos = h.infos();
        assert!(infos.len() >= 4);
        for i in &infos {
            assert_eq!(i.error, None, "{}", i.id);
        }
        let x = h.export_start(&job()).unwrap();
        assert_eq!(
            x.vf,
            [
                "drawtext=expansion=none:text=it\\\\\\'s 50%\\\\: ok:fontsize=h*0.04:fontcolor=white@0.8:\
              borderw=2:bordercolor=black@0.5:x=w-tw-24:y=h-th-24"
            ]
        );
    }

    #[test]
    fn bookmarks_example() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins");
        let mut h = Host::load_from(dir, &Settings::default());
        let menu = h
            .menus(Screen::Editor)
            .into_iter()
            .find(|m| m.title == "Bookmarks")
            .unwrap();
        let item = |label: &str| {
            menu.entries
                .iter()
                .find_map(|e| match e {
                    MenuEntry::Item(i) if i.label == label => Some((i.plugin, i.item, i.shortcut)),
                    _ => None,
                })
                .unwrap()
        };
        assert!(item("Next bookmark").2.is_some()); // Alt+ArrowRight parses
        let tmp = std::env::temp_dir().join(format!("clipman-bm-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let clip = tmp.join("c.mp4");
        let ctx = |playhead: f64| Ctx {
            screen: Screen::Editor,
            clip: Some(ClipState {
                path: clip.clone(),
                duration: 60.0,
                playhead,
                ..Default::default()
            }),
        };
        let run = |h: &mut Host, label: &str, t: f64| {
            let (p, i, _) = item(label);
            h.run_menu(p, i, &ctx(t));
            h.take_clip_cmds()
        };
        run(&mut h, "Bookmark here", 30.0);
        run(&mut h, "Bookmark here", 10.0);
        assert_eq!(super::super::meta::read(&clip, "bookmarks").unwrap()["marks"], serde_json::json!([10.0, 30.0]));
        assert_eq!(run(&mut h, "Next bookmark", 10.0), [ClipCmd::Seek(30.0)]);
        assert_eq!(run(&mut h, "Previous bookmark", 30.0), [ClipCmd::Seek(10.0)]);
        assert_eq!(run(&mut h, "Next bookmark", 30.0), []);
        assert_eq!(h.take_notices(), ["bookmarks: No bookmark after the playhead"]);
        assert_eq!(
            run(&mut h, "Trim around nearest bookmark", 28.0),
            [ClipCmd::SetRange(20.0, 35.0), ClipCmd::Seek(20.0)]
        );
        run(&mut h, "Remove nearest bookmark", 12.0);
        assert_eq!(super::super::meta::read(&clip, "bookmarks").unwrap()["marks"], serde_json::json!([30.0]));
        run(&mut h, "Remove all bookmarks", 0.0);
        assert!(!super::super::meta::sidecar_path(&clip).exists());
        assert!(h.infos().iter().all(|i| i.error.is_none()));
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn sidecar_json_and_spawn() {
        let (mut h, dir) = host_with(
            "meta",
            r#"
            return {
              menus = { { title = "T", items = { { label = "Go", run = function()
                local clip = clipman.plugin_dir .. "/clip.mp4"
                clipman.meta.set(clip, { n = 1, list = { "a", "b" } })
                local m = clipman.meta.get(clip)
                clipman.notify(clipman.json.encode(m))
                clipman.notify(clipman.json.encode(clipman.json.decode('{"x":[1,2]}')))
                clipman.spawn{ args = { "sh", "-c", "cat; echo out; exit 3" }, stdin = "in ",
                  on_exit = function(code, out, err) clipman.notify(code .. ":" .. out) end }
              end } } } },
              badge = function(path) return clipman.meta.get(path) and "tagged" or nil end,
            }
            "#,
        );
        h.run_menu(0, 0, &Ctx::default());
        let clip = dir.join("meta").join("clip.mp4");
        assert_eq!(
            h.take_notices(),
            [r#"meta: {"list":["a","b"],"n":1}"#, r#"meta: {"x":[1,2]}"#]
        );
        assert!(super::super::meta::sidecar_path(&clip).is_file());
        assert_eq!(h.badges(&clip), ["tagged"]);
        assert!(h.badges(&dir.join("other.mp4")).is_empty());
        #[cfg(unix)]
        {
            assert!(h.busy());
            h.wait_idle();
            assert_eq!(h.take_notices(), ["meta: 3:in out\n"]);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
