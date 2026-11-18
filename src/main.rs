#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, Key, Modifiers, Pos2, Rect, TextureHandle, TextureOptions};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// 同時に保持するテクスチャのメモリ予算 (バイト)。
const TEXTURE_BUDGET_BYTES: usize = 1024 * 1024 * 1024;
const MIN_CACHE_FRAMES: usize = 16;
const MAX_CACHE_FRAMES: usize = 600;
/// 画像サイズが分かるまでの仮のキャッシュ枚数。
const DEFAULT_CACHE_FRAMES: usize = 64;
/// 1回の update で GPU にアップロードする最大枚数。
const MAX_UPLOADS_PER_UPDATE: usize = 4;
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "bmp", "webp", "gif", "tif", "tiff"];
const SPEED_PRESETS: &[f64] = &[0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0];

struct Request {
    generation: u64,
    index: usize,
    path: PathBuf,
}

struct Response {
    generation: u64,
    index: usize,
    result: Result<egui::ColorImage, String>,
}

fn spawn_workers(
    current_generation: Arc<AtomicU64>,
    ctx: egui::Context,
) -> (Sender<Request>, Receiver<Response>) {
    let (req_tx, req_rx) = channel::<Request>();
    let (res_tx, res_rx) = channel::<Response>();
    let req_rx = Arc::new(Mutex::new(req_rx));
    let threads = num_cpus::get().saturating_sub(1).clamp(2, 8);
    for _ in 0..threads {
        let req_rx = Arc::clone(&req_rx);
        let res_tx = res_tx.clone();
        let current_generation = Arc::clone(&current_generation);
        let ctx = ctx.clone();
        std::thread::spawn(move || loop {
            let request = {
                let guard = match req_rx.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                match guard.recv() {
                    Ok(r) => r,
                    Err(_) => return,
                }
            };
            // ディレクトリが切り替わった後の古い要求は読み込まずに破棄する。
            if request.generation != current_generation.load(AtomicOrdering::Relaxed) {
                continue;
            }
            let result = image::open(&request.path)
                .map(|img| {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw())
                })
                .map_err(|e| format!("{}: {}", request.path.display(), e));
            let response = Response {
                generation: request.generation,
                index: request.index,
                result,
            };
            if res_tx.send(response).is_err() {
                return;
            }
            ctx.request_repaint();
        });
    }
    (req_tx, res_rx)
}

/// "frame2.png" < "frame10.png" となるように、数字部分を数値として比較する。
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let si = i;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            let sj = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let na: String = a[si..i].iter().collect::<String>().trim_start_matches('0').to_string();
            let nb: String = b[sj..j].iter().collect::<String>().trim_start_matches('0').to_string();
            let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(&nb));
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            let ord = a[i].cmp(&b[j]);
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

fn list_subdirs(root: &Path) -> Vec<String> {
    let mut dirs: Vec<String> = std::fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort_by(|a, b| natural_cmp(a, b));
    dirs
}

fn list_images(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .and_then(|e| e.to_str())
                            .map(|e| IMAGE_EXTENSIONS.contains(&e.to_lowercase().as_str()))
                            .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort_by(|a, b| {
        natural_cmp(
            &a.file_name().unwrap_or_default().to_string_lossy(),
            &b.file_name().unwrap_or_default().to_string_lossy(),
        )
    });
    files
}

fn find_root() -> PathBuf {
    if let Some(arg) = std::env::args().nth(1) {
        return PathBuf::from(arg);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("frame"));
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(5) {
            candidates.push(ancestor.join("frame"));
        }
    }
    candidates
        .iter()
        .find(|p| p.is_dir())
        .cloned()
        .unwrap_or_else(|| candidates.into_iter().next().unwrap_or_else(|| PathBuf::from("frame")))
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:/Windows/Fonts/meiryo.ttc",
        "C:/Windows/Fonts/YuGothM.ttc",
        "C:/Windows/Fonts/YuGothR.ttc",
        "C:/Windows/Fonts/msgothic.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("jp".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(family).or_default().push("jp".to_owned());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

struct Player {
    root: PathBuf,
    dirs: Vec<String>,
    selected: Option<usize>,
    files: Arc<Vec<PathBuf>>,

    textures: HashMap<usize, TextureHandle>,
    in_flight: HashSet<usize>,
    failed: HashSet<usize>,
    last_error: Option<String>,
    cache_capacity: usize,
    size_known: bool,
    last_shown: Option<usize>,

    generation: u64,
    current_generation: Arc<AtomicU64>,
    req_tx: Sender<Request>,
    res_rx: Receiver<Response>,
    max_in_flight: usize,

    /// 現在位置 (フレーム単位、小数を含む)。
    pos: f64,
    playing: bool,
    reverse: bool,
    speed: f64,
    fps: f64,
    looping: bool,
    buffering: bool,
    seeking: bool,
}

impl Player {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        let current_generation = Arc::new(AtomicU64::new(0));
        let (req_tx, res_rx) = spawn_workers(Arc::clone(&current_generation), cc.egui_ctx.clone());
        let root = find_root();
        let mut player = Self {
            dirs: list_subdirs(&root),
            root,
            selected: None,
            files: Arc::new(Vec::new()),
            textures: HashMap::new(),
            in_flight: HashSet::new(),
            failed: HashSet::new(),
            last_error: None,
            cache_capacity: DEFAULT_CACHE_FRAMES,
            size_known: false,
            last_shown: None,
            generation: 0,
            current_generation,
            req_tx,
            res_rx,
            max_in_flight: num_cpus::get().clamp(2, 8) * 2,
            pos: 0.0,
            playing: false,
            reverse: false,
            speed: 1.0,
            fps: 24.0,
            looping: true,
            buffering: false,
            seeking: false,
        };
        if !player.dirs.is_empty() {
            player.select_dir(0);
        }
        player
    }

    fn select_dir(&mut self, index: usize) {
        let Some(name) = self.dirs.get(index) else { return };
        let path = self.root.join(name);
        self.selected = Some(index);
        self.files = Arc::new(list_images(&path));
        self.generation += 1;
        self.current_generation.store(self.generation, AtomicOrdering::Relaxed);
        self.textures.clear();
        self.in_flight.clear();
        self.failed.clear();
        self.last_error = None;
        self.cache_capacity = DEFAULT_CACHE_FRAMES;
        self.size_known = false;
        self.last_shown = None;
        self.pos = 0.0;
        self.playing = !self.files.is_empty();
        self.reverse = false;
        self.buffering = false;
    }

    fn frame_count(&self) -> usize {
        self.files.len()
    }

    fn current_index(&self) -> usize {
        let n = self.frame_count();
        if n == 0 {
            0
        } else {
            (self.pos.floor().max(0.0) as usize).min(n - 1)
        }
    }

    fn direction(&self) -> i64 {
        if self.reverse { -1 } else { 1 }
    }

    /// 再生中に 1 回の画面更新で進むフレーム数の目安。先読みの間隔に使う。
    fn stride(&self) -> usize {
        if self.playing {
            ((self.fps * self.speed / 60.0).round() as usize).max(1)
        } else {
            1
        }
    }

    /// 指定フレームの代わりに表示できるフレームを返す。
    /// 指定フレーム自体が読み込み済みならそれを返し、なければ近傍の読み込み済みフレームを探す。
    fn displayable(&self, index: usize, radius: usize) -> Option<usize> {
        if self.textures.contains_key(&index) {
            return Some(index);
        }
        let radius = if self.failed.contains(&index) { radius.max(64) } else { radius };
        let n = self.frame_count() as i64;
        for d in 1..=radius as i64 {
            for candidate in [index as i64 - d * self.direction(), index as i64 + d * self.direction()] {
                if candidate >= 0 && candidate < n && self.textures.contains_key(&(candidate as usize)) {
                    return Some(candidate as usize);
                }
            }
        }
        None
    }

    fn advance_clock(&mut self, dt: f64) {
        let n = self.frame_count();
        if !self.playing || n == 0 || self.seeking {
            self.buffering = false;
            return;
        }
        let step = dt * self.fps * self.speed * self.direction() as f64;
        let mut next = self.pos + step;
        let last = (n - 1) as f64;
        if self.looping {
            // 末尾を越えたら先頭へ、先頭を越えたら末尾へ戻る。
            next = next.rem_euclid(n as f64);
        } else if next >= last && !self.reverse {
            next = last;
            self.playing = false;
        } else if next <= 0.0 && self.reverse {
            next = 0.0;
            self.playing = false;
        }
        let target = (next.floor().max(0.0) as usize).min(n - 1);
        if self.displayable(target, self.stride()).is_some() || !self.playing {
            self.pos = next;
            self.buffering = false;
        } else {
            // 次のフレームが未読み込みの場合は時間を進めず、読み込み完了を待つ。
            self.buffering = true;
        }
    }

    fn receive_results(&mut self, ctx: &egui::Context, desired: &HashSet<usize>) {
        for _ in 0..MAX_UPLOADS_PER_UPDATE {
            let Ok(resp) = self.res_rx.try_recv() else { return };
            if resp.generation != self.generation {
                continue;
            }
            self.in_flight.remove(&resp.index);
            match resp.result {
                Ok(image) => {
                    if !self.size_known {
                        let bytes = (image.size[0] * image.size[1] * 4).max(1);
                        self.cache_capacity =
                            (TEXTURE_BUDGET_BYTES / bytes).clamp(MIN_CACHE_FRAMES, MAX_CACHE_FRAMES);
                        self.size_known = true;
                    }
                    if desired.contains(&resp.index) {
                        let tex = ctx.load_texture(format!("frame{}", resp.index), image, TextureOptions::LINEAR);
                        self.textures.insert(resp.index, tex);
                    }
                }
                Err(msg) => {
                    self.failed.insert(resp.index);
                    self.last_error = Some(msg);
                }
            }
        }
        // 未処理の結果が残っている場合は、次の画面更新で続きを処理する。
        ctx.request_repaint();
    }

    /// 現在位置の周辺で保持したいフレームを、読み込みの優先順に並べて返す。
    fn desired_frames(&self) -> Vec<usize> {
        let n = self.frame_count();
        if n == 0 {
            return Vec::new();
        }
        let cur = self.current_index() as i64;
        let dir = if self.playing { self.direction() } else { 1 };
        let cap = self.cache_capacity.min(n);
        let (ahead, behind) = if self.playing { (cap * 3 / 4, cap / 4) } else { (cap / 2, cap / 2) };
        let stride = self.stride() as i64;
        let wrap = |i: i64| -> Option<usize> {
            if self.looping {
                Some(i.rem_euclid(n as i64) as usize)
            } else if i >= 0 && i < n as i64 {
                Some(i as usize)
            } else {
                None
            }
        };
        let mut seen = HashSet::new();
        let mut order = Vec::new();
        let mut push = |idx: Option<usize>| {
            if let Some(i) = idx {
                if order.len() < cap && seen.insert(i) {
                    order.push(i);
                }
            }
        };
        // 再生方向の先読みでは、まず stride 間隔のフレームを優先し、残りを後から埋める。
        for k in (0..ahead as i64).step_by(stride as usize) {
            push(wrap(cur + dir * k));
        }
        for k in 0..ahead as i64 {
            push(wrap(cur + dir * k));
        }
        for k in 1..=behind as i64 {
            push(wrap(cur - dir * k));
        }
        order
    }

    fn schedule_loading(&mut self, order: &[usize], desired: &HashSet<usize>) {
        self.textures.retain(|i, _| desired.contains(i));
        self.in_flight.retain(|i| desired.contains(i));
        for &idx in order {
            if self.in_flight.len() >= self.max_in_flight {
                break;
            }
            if self.textures.contains_key(&idx) || self.in_flight.contains(&idx) || self.failed.contains(&idx) {
                continue;
            }
            let request = Request {
                generation: self.generation,
                index: idx,
                path: self.files[idx].clone(),
            };
            if self.req_tx.send(request).is_ok() {
                self.in_flight.insert(idx);
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let n = self.frame_count();
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::Space) {
                self.playing = !self.playing;
            }
            if i.consume_key(Modifiers::NONE, Key::R) {
                self.reverse = !self.reverse;
            }
            if i.consume_key(Modifiers::NONE, Key::L) {
                self.looping = !self.looping;
            }
            if n > 0 {
                if i.consume_key(Modifiers::NONE, Key::ArrowLeft) {
                    self.playing = false;
                    self.pos = (self.current_index().saturating_sub(1)) as f64;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowRight) {
                    self.playing = false;
                    self.pos = (self.current_index() + 1).min(n - 1) as f64;
                }
                if i.consume_key(Modifiers::NONE, Key::Home) {
                    self.pos = 0.0;
                }
                if i.consume_key(Modifiers::NONE, Key::End) {
                    self.pos = (n - 1) as f64;
                }
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                self.speed = (self.speed * 2.0).min(32.0);
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                self.speed = (self.speed / 2.0).max(0.01);
            }
        });
    }

    fn controls_ui(&mut self, ui: &mut egui::Ui) {
        let n = self.frame_count();

        // ディレクトリ選択
        ui.horizontal(|ui| {
            ui.label("ディレクトリ:");
            let selected_text = self
                .selected
                .and_then(|i| self.dirs.get(i).cloned())
                .unwrap_or_else(|| "(なし)".to_owned());
            let mut chosen = None;
            let mut reload = false;
            egui::ComboBox::from_id_salt("dir_select")
                .selected_text(selected_text)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for (i, name) in self.dirs.iter().enumerate() {
                        if ui.selectable_label(self.selected == Some(i), name).clicked() {
                            chosen = Some(i);
                        }
                    }
                });
            if ui.button("前").clicked() {
                if let Some(i) = self.selected {
                    chosen = Some(i.saturating_sub(1));
                }
            }
            if ui.button("次").clicked() {
                if let Some(i) = self.selected {
                    chosen = Some((i + 1).min(self.dirs.len().saturating_sub(1)));
                }
            }
            if ui.button("再読み込み").clicked() {
                // ディレクトリ一覧を取り直し、選択中のディレクトリの画像一覧も読み直す。
                reload = true;
                let keep = self.selected.and_then(|i| self.dirs.get(i).cloned());
                self.dirs = list_subdirs(&self.root);
                chosen = keep
                    .and_then(|name| self.dirs.iter().position(|d| *d == name))
                    .or(if self.dirs.is_empty() { None } else { Some(0) });
                if chosen.is_none() {
                    self.selected = None;
                }
            }
            if let Some(i) = chosen {
                if self.selected != Some(i) || reload {
                    self.select_dir(i);
                }
            }
            ui.label(egui::RichText::new(self.root.display().to_string()).weak());
        });

        // シークバー
        ui.horizontal(|ui| {
            let mut idx = self.current_index();
            let max = n.saturating_sub(1);
            ui.spacing_mut().slider_width = (ui.available_width() - 150.0).max(100.0);
            let resp = ui.add_enabled(
                n > 0,
                egui::Slider::new(&mut idx, 0..=max).show_value(false),
            );
            self.seeking = resp.dragged();
            if resp.changed() {
                self.pos = idx as f64;
            }
            let total_secs = n as f64 / self.fps;
            ui.monospace(format!(
                "{:>5}/{:<5} {:6.2}s/{:.2}s",
                if n == 0 { 0 } else { self.current_index() + 1 },
                n,
                self.pos / self.fps,
                total_secs
            ));
        });

        // 再生操作
        ui.horizontal_wrapped(|ui| {
            if ui.button("先頭").clicked() {
                self.pos = 0.0;
            }
            if ui.button("コマ戻し").clicked() && n > 0 {
                self.playing = false;
                self.pos = self.current_index().saturating_sub(1) as f64;
            }
            let reverse_active = self.playing && self.reverse;
            if ui.selectable_label(reverse_active, "逆再生").clicked() {
                self.reverse = true;
                self.playing = true;
            }
            let play_label = if self.playing { "一時停止" } else { "再生" };
            if ui.button(play_label).clicked() {
                if self.playing {
                    self.playing = false;
                } else {
                    self.playing = true;
                    // 一時停止から再開した場合は、直前の再生方向を維持する。
                }
            }
            let forward_active = self.playing && !self.reverse;
            if ui.selectable_label(forward_active, "順再生").clicked() {
                self.reverse = false;
                self.playing = true;
            }
            if ui.button("コマ送り").clicked() && n > 0 {
                self.playing = false;
                self.pos = (self.current_index() + 1).min(n - 1) as f64;
            }
            if ui.button("末尾").clicked() && n > 0 {
                self.pos = (n - 1) as f64;
            }
            ui.separator();
            ui.checkbox(&mut self.looping, "ループ");
        });

        // 速度とFPS
        ui.horizontal_wrapped(|ui| {
            ui.label("速度:");
            for &preset in SPEED_PRESETS {
                let selected = (self.speed - preset).abs() < 1e-9;
                if ui.selectable_label(selected, format!("{preset}x")).clicked() {
                    self.speed = preset;
                }
            }
            ui.add(
                egui::DragValue::new(&mut self.speed)
                    .range(0.01..=32.0)
                    .speed(0.02)
                    .suffix("x"),
            );
            ui.separator();
            ui.label("基準FPS:");
            ui.add(egui::DragValue::new(&mut self.fps).range(1.0..=240.0).speed(0.2));
            ui.separator();
            ui.label(format!(
                "実効 {:.1} fps ({})",
                self.fps * self.speed,
                if self.reverse { "逆方向" } else { "順方向" }
            ));
        });

        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(
                    "キー: Space 再生/停止  R 方向反転  ←/→ コマ送り  ↑/↓ 速度2倍/半分  Home/End 先頭/末尾  L ループ",
                )
                .weak(),
            );
            ui.label(egui::RichText::new(format!("キャッシュ {}/{} 枚", self.textures.len(), self.cache_capacity)).weak());
            if self.buffering {
                ui.colored_label(Color32::YELLOW, "読み込み待ち");
            }
            if !self.failed.is_empty() {
                ui.colored_label(Color32::LIGHT_RED, format!("読み込み失敗 {} 枚", self.failed.len()));
            }
        });
        if let Some(err) = &self.last_error {
            ui.colored_label(Color32::LIGHT_RED, err);
        }
    }

    fn image_ui(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);

        let shown = self
            .displayable(self.current_index(), self.stride())
            .or(self.last_shown.filter(|i| self.textures.contains_key(i)));
        if let Some(idx) = shown {
            self.last_shown = Some(idx);
            let tex = &self.textures[&idx];
            let size = tex.size_vec2();
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let draw = Rect::from_center_size(rect.center(), size * scale);
            ui.painter().image(
                tex.id(),
                draw,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            let message = if self.dirs.is_empty() {
                format!("{} にディレクトリがありません", self.root.display())
            } else if self.frame_count() == 0 {
                "このディレクトリに画像がありません".to_owned()
            } else {
                "読み込み中...".to_owned()
            };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                message,
                egui::FontId::proportional(18.0),
                Color32::GRAY,
            );
        }
    }
}

impl eframe::App for Player {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_keys(ctx);
        let dt = ctx.input(|i| i.stable_dt).min(0.1) as f64;
        self.advance_clock(dt);

        let order = self.desired_frames();
        let desired: HashSet<usize> = order.iter().copied().collect();
        self.receive_results(ctx, &desired);
        self.schedule_loading(&order, &desired);

        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            ui.add_space(4.0);
            self.controls_ui(ui);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| self.image_ui(ui));

        if self.playing || !self.in_flight.is_empty() {
            ctx.request_repaint();
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 780.0])
            .with_title("Frame Player"),
        ..Default::default()
    };
    eframe::run_native(
        "Frame Player",
        options,
        Box::new(|cc| Ok(Box::new(Player::new(cc)))),
    )
}
