// BulkReName GUI using eframe (egui) + rfd for file dialog.

#![windows_subsystem = "windows"]

use chrono::{DateTime, Local};
use directories::ProjectDirs;
use eframe::{egui, egui::RichText};
use egui::{ComboBox, DragValue};//, Layout};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use eframe::egui::ViewportBuilder;
use image::GenericImageView;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Block {
    Literal(String),
    Number { width: usize, start: i64, step: i64 },
    Date { format: String },
    Original { mode: OriginalMode, },
    Extension,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum OriginalMode {
    Keep,
    RemoveRange { start: i32, end: i32, },
    RemoveSubstring { pattern: String, case_sensitive: bool, },
}

#[derive(Clone)]
struct FileEntry {
    path: PathBuf,
}

#[derive(PartialEq, Copy, Clone, Serialize, Deserialize)]
enum CollisionStrategy {
    Overwrite,
    Skip,
    Suffix,
}

#[derive(Serialize, Deserialize)]
struct Template {
    name: String,
    blocks: Vec<Block>,
    collision: CollisionStrategy,
    use_mtime_for_date: bool,
}

enum ThumbnailState {
    Loading,
    Loaded(egui::TextureHandle, egui::Vec2),
    Failed,
}

#[derive(PartialEq)]
enum LoadingPhase {
    None,
    AddingFiles,
    LoadingThumbs,
}

//sort
#[derive(PartialEq, Copy, Clone)]
enum SortKey {
    Name,
    Modified,
    Created,
    Size,
}
#[derive(PartialEq, Copy, Clone)]
enum SortOrder {
    Asc,
    Desc,
}

struct BulkRename {
    files: Vec<FileEntry>,
    selected_idx: Option<usize>,
    blocks: Vec<Block>,
    collision: CollisionStrategy,
    use_mtime_for_date: bool,
    last_actions: Vec<HashMap<PathBuf, PathBuf>>,
    messages: Vec<String>,
    dragging_idx: Option<usize>,
    // thumbnail cache: key = path → state
    thumbnails: HashMap<String, ThumbnailState>,
    thumb_max_size: (usize, usize),
    thumb_tx: Option<SyncSender<(String, Result<(image::RgbaImage, (usize, usize)), String>)>>,
    thumb_rx: Option<Receiver<(String, Result<(image::RgbaImage, (usize, usize)), String>)>>,
    show_thumbnails: bool,
    // persistence
    saved_templates: Vec<Template>,
    current_template_name: String,
    //loading
    loading_phase: LoadingPhase,
    loader_rx: Option<Receiver<PathBuf>>,
    loading_count: usize,
    //sort
    sort_key: Option<SortKey>,
    sort_order: SortOrder,
    //error
    show_delete_error: bool,
}

impl Default for BulkRename {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            selected_idx: None,
            blocks: vec![
                Block::Original { mode: OriginalMode::Keep },
                Block::Extension,
            ],
            collision: CollisionStrategy::Suffix,
            use_mtime_for_date: true,
            last_actions: Vec::new(),
            messages: Vec::new(),
            dragging_idx: None,
            thumbnails: HashMap::new(),
            thumb_max_size: (160, 120),
            thumb_tx: None,
            thumb_rx: None,
            show_thumbnails: true,
            saved_templates: Vec::new(),
            current_template_name: String::new(),
            //loading
            loading_phase: LoadingPhase::None,
            loader_rx: None,
            loading_count: 0,
            //sort
            sort_key: None,
            sort_order: SortOrder::Asc,
            //error
            show_delete_error: false,
        }
    }
}

impl BulkRename {
    /// Path to `templates.json` in user config directory.
    fn config_path() -> PathBuf {
        let proj = ProjectDirs::from("jp", "mi3zuk", "BulkReName")
            .expect("failed to get project directory");
        let dir = proj.config_dir();
        let _ = fs::create_dir_all(dir);
        dir.join("templates.json")
    }

    fn load_templates(&mut self) {
        if let Ok(text) = fs::read_to_string(Self::config_path()) {
            if let Ok(list) = serde_json::from_str::<Vec<Template>>(&text) {
                self.saved_templates = list;
            }
        }
    }

    fn save_templates(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.saved_templates) {
            let _ = fs::write(Self::config_path(), json);
        }
    }

    fn add_files(&mut self, paths: Vec<PathBuf>) {
        for p in paths {
            if p.is_file() {
                self.files.push(FileEntry { path: p });
            }
        }
    }

    fn process_original(name: &str, mode: &OriginalMode) -> String {
        match mode {
            OriginalMode::Keep => name.to_string(),

            OriginalMode::RemoveSubstring { pattern, case_sensitive } => {
                let mut result = name.to_string();

                for p in pattern.split('/') {
                    let p = p.trim();

                    // 空要素無視
                    if p.is_empty() {
                        continue;
                    }

                    if *case_sensitive {
                        result = result.replace(p, "");
                    } else {
                        // case-insensitive replace
                        let mut tmp = String::new();
                        let mut rest = result.as_str();

                        let p_lower = p.to_lowercase();

                        while let Some(pos) = rest.to_lowercase().find(&p_lower) {
                            tmp.push_str(&rest[..pos]);
                            rest = &rest[pos + p.len()..];
                        }
                        tmp.push_str(rest);
                        result = tmp;
                    }
                }
                result
            }

            OriginalMode::RemoveRange { start, end } => {
                let chars: Vec<char> = name.chars().collect();
                let len = chars.len() as i32;
                let s = if *start < 0 { len + *start } else { *start };
                let e = if *end < 0 { len + *end + 1 } else { *end };
                let s = s.clamp(0, len);
                let e = e.clamp(0, len);
                if s >= e {
                    return name.to_string();
                }
                chars[..s as usize]
                    .iter()
                    .chain(chars[e as usize..].iter())
                    .collect()
            }
        }
    }

    fn sort_files(&mut self, key: SortKey) {
        if self.sort_key == Some(key) {
            self.sort_order = match self.sort_order {
                SortOrder::Asc => SortOrder::Desc,
                SortOrder::Desc => SortOrder::Asc,
            };
        } else {
            self.sort_key = Some(key);
            self.sort_order = SortOrder::Asc;
        }
        let asc = self.sort_order == SortOrder::Asc;
        match key {
            SortKey::Name => {
                self.files.sort_by(|a, b| {
                    let ord = a.path.file_name().cmp(&b.path.file_name());
                    if asc { ord } else { ord.reverse() }
                });
            }
            SortKey::Modified => {
                self.files.sort_by(|a, b| {
                    let a_m = fs::metadata(&a.path).and_then(|m| m.modified()).ok();
                    let b_m = fs::metadata(&b.path).and_then(|m| m.modified()).ok();
                    let ord = a_m.cmp(&b_m);
                    if asc { ord } else { ord.reverse() }
                });
            }
            SortKey::Created => {
                self.files.sort_by(|a, b| {
                    let a_c = fs::metadata(&a.path).and_then(|m| m.created()).ok();
                    let b_c = fs::metadata(&b.path).and_then(|m| m.created()).ok();
                    let ord = a_c.cmp(&b_c);
                    if asc { ord } else { ord.reverse() }
                });
            }
            SortKey::Size => {
                self.files.sort_by(|a, b| {
                    let a_s = fs::metadata(&a.path).map(|m| m.len()).ok();
                    let b_s = fs::metadata(&b.path).map(|m| m.len()).ok();
                    let ord = a_s.cmp(&b_s);
                    if asc { ord } else { ord.reverse() }
                });
            }
        }
    }

    fn collect_files_recursively(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(read_dir) = fs::read_dir(dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.is_file() {
                    out.push(path);
                } else if path.is_dir() {
                    Self::collect_files_recursively(&path, out);
                }
            }
        }
    }

    fn move_up(&mut self) {
        if let Some(i) = self.selected_idx {
            if i > 0 {
                self.files.swap(i, i - 1);
                self.selected_idx = Some(i - 1);
            }
        }
    }

    fn move_down(&mut self) {
        if let Some(i) = self.selected_idx {
            if i + 1 < self.files.len() {
                self.files.swap(i, i + 1);
                self.selected_idx = Some(i + 1);
            }
        }
    }

    fn remove_selected(&mut self) {
        if let Some(i) = self.selected_idx {
            if let Some(p) = self.files.get(i) {
                let key = p.path.to_string_lossy().to_string();
                self.thumbnails.remove(&key);
            }
            self.files.remove(i);
            self.selected_idx = None;
        }
    }

    fn format_number(&self, idx: usize, width: usize, start: i64, step: i64) -> String {
        let val = start + (idx as i64) * step;
        let s = format!("{}", val);
        if width > 0 && s.len() < width {
            format!("{:0width$}", val, width = width)
        } else {
            s
        }
    }

    fn generate_targets(&self) -> Vec<String> {
        let mut res = Vec::new();
        for (idx, fe) in self.files.iter().enumerate() {
            let file_name = fe
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let ext = fe
                .path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            let now: DateTime<Local> = Local::now();
            let mut parts = Vec::new();
            for b in &self.blocks {
                match b {
                    Block::Literal(s) => parts.push(s.clone()),
                    Block::Number { width, start, step } => {
                        parts.push(self.format_number(idx, *width, *start, *step))
                    }
                    Block::Date { format } => {
                        let s = std::panic::catch_unwind(|| {
                            now.format(format).to_string()
                        })
                        .unwrap_or_else(|_| "[INVALID_DATE]".to_string());
                        parts.push(s);
                    }
                    Block::Original { mode } => {
                        parts.push(Self::process_original(&file_name, mode));
                    }
                    Block::Extension => {
                        if !ext.is_empty() {
                            parts.push(format!(".{}", ext));
                        }
                    }
                }
            }
            res.push(parts.join(""));
        }
        res
    }

    fn preview_table(&self) -> Vec<(String, String)> {
        let targets = self.generate_targets();
        self.files
            .iter()
            .zip(targets.iter())
            .map(|(f, t)| {
                (
                    f.path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string(),
                    t.clone(),
                )
            })
            .collect()
    }

    fn ensure_thumbnail(&mut self, _ctx: &egui::Context, path: &Path) {
        let key = path.to_string_lossy().to_string();
        if self.thumbnails.contains_key(&key) {
            return;
        }
        if let Some(ext) = path.extension().and_then(|s| s.to_str()).map(|s| s.to_lowercase()) {
            let supported = ["png", "jpg", "jpeg", "webp", "gif", "bmp", "ico"];
            if !supported.contains(&ext.as_str()) {
                return;
            }
        } else {
            return;
        }
        self.thumbnails.insert(key.clone(), ThumbnailState::Loading);

        // Initialize channel if not already done
        if self.thumb_tx.is_none() {
            let (tx, rx) = mpsc::sync_channel(4); // Limit to 4 concurrent thumbnail threads
            self.thumb_tx = Some(tx);
            self.thumb_rx = Some(rx);
        }

        let path_clone = path.to_path_buf();
        let max_size = self.thumb_max_size;
        if let Some(tx) = &self.thumb_tx {
            let tx_clone = tx.clone();
            thread::spawn(move || {
                let result = std::panic::catch_unwind(|| {
                    match image::open(&path_clone) {
                        Ok(img) => {
                            let (max_w, max_h) = max_size;
                            let thumb = img.thumbnail(max_w as u32, max_h as u32).into_rgba8();
                            let (w, h) = (thumb.width() as usize, thumb.height() as usize);
                            Ok((thumb, (w, h)))
                        }
                        Err(e) => Err(format!("{:?}", e)),
                    }
                }).unwrap_or_else(|_| Err("Panic in image processing".to_string()));
                tx_clone.send((key, result)).ok();
            });
        }
    }

    fn split_stem_and_number(stem: &str) -> (String, Option<u32>) {
        if let Some(idx) = stem.rfind('(') {
            if stem.ends_with(')') {
                let num_part = &stem[idx + 1..stem.len() - 1];
                if let Ok(n) = num_part.parse::<u32>() {
                    let base = stem[..idx].trim_end().to_string();
                    return (base, Some(n));
                }
            }
        }
        (stem.to_string(), None)
    }

    fn make_numbered_path(path: &PathBuf, n: u32) -> PathBuf {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let ext = path.extension().and_then(|s| s.to_str());
        let dir = path.parent().unwrap_or(Path::new("."));

        let (base, _) = Self::split_stem_and_number(stem);
        let new_stem = format!("{} ({})", base, n);

        if let Some(e) = ext {
            dir.join(format!("{}.{}", new_stem, e))
        } else {
            dir.join(new_stem)
        }
    }

    fn execute_rename(&mut self) {
        let targets = self.generate_targets();
        if targets.len() != self.files.len() {
            return;
        }

        // final_paths creation
        let mut final_paths = Vec::new();
        for (fe, tname) in self.files.iter().zip(targets.iter()) {
            let mut p = fe.path.clone();
            p.set_file_name(tname);
            final_paths.push(p);
        }

        // Duplicate detection (between final entries)
        use std::collections::{HashMap, HashSet};

        let mut used = HashSet::new();
        let mut resolved_paths = Vec::new();

        for (i, path) in final_paths.iter().enumerate() {
            let orig = &self.files[i].path;

            match self.collision {
                CollisionStrategy::Overwrite => {
                    resolved_paths.push(path.clone());
                }

                CollisionStrategy::Skip => {
                    if used.contains(path) {
                        resolved_paths.push(orig.clone());
                    } else {
                        used.insert(path.clone());
                        resolved_paths.push(path.clone());
                    }
                }

                CollisionStrategy::Suffix => {
                    let mut candidate = path.clone();
                    let mut n = 1;

                    while used.contains(&candidate) {
                        candidate = Self::make_numbered_path(path, n);
                        n += 1;
                    }

                    used.insert(candidate.clone());
                    resolved_paths.push(candidate);
                }
            }
        }


        // orig -> tmp -> final
        let mut robust_map = Vec::new();

        for (i, fe) in self.files.iter().enumerate() {
            let orig = fe.path.clone();
            let desired = resolved_paths[i].clone();

            if orig == desired {
                continue;
            }

            let dir = orig.parent().unwrap_or(Path::new("."));
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();

            let tmp = dir.join(format!(".tmp-{}-{}.tmp", nanos, i));
            robust_map.push((orig, tmp, desired));
        }

        if robust_map.is_empty() {
            self.messages.push("No files to rename.".into());
            return;
        }

        // orig → tmp
        let mut temps_created = Vec::new();
        for (orig, tmp, _) in &robust_map {
            if let Err(e) = fs::rename(orig, tmp) {
                self.messages.push(format!("Failed: {}", e));
                for (t, o) in temps_created.iter().rev() {
                    let _ = fs::rename(t, o);
                }
                return;
            }
            temps_created.push((tmp.clone(), orig.clone()));
        }

        // tmp → final
        for (_orig, tmp, final_path) in &robust_map {
            if let Err(e) = fs::rename(tmp, final_path) {
                self.messages.push(format!("Failed final rename: {}", e));
                return;
            }
        }

        // undo
        let mut undo_map = HashMap::new();
        for (orig, _tmp, final_path) in robust_map {
            undo_map.insert(orig, final_path);
        }
        self.last_actions.push(undo_map);

        self.messages.push("Rename completed.".into());
    }

    fn undo(&mut self) {
        if let Some(mapping) = self.last_actions.pop() {
            for (orig, final_path) in mapping {
                if final_path.exists() {
                    if let Err(e) = fs::rename(&final_path, &orig) {
                        self.messages.push(format!(
                            "Failed to undo {:?} -> {:?}: {}",
                            final_path, orig, e
                        ));
                    }
                } else {
                    self.messages.push(format!(
                        "Cannot undo, final file missing: {:?}",
                        final_path
                    ));
                }
            }
            self.messages.push("Undo attempted.".into());
        } else {
            self.messages.push("No actions to undo.".into());
        }
    }
}

impl eframe::App for BulkRename {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Process thumbnail loading results
        if let Some(rx) = &self.thumb_rx {
            while let Ok((key, result)) = rx.try_recv() {
                match result {
                    Ok((rgba, (w, h))) => {
                        let color_image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba.into_vec());
                        let tex = ctx.load_texture(key.clone(), color_image, egui::TextureOptions::NEAREST);
                        let orig_size = egui::Vec2::new(w as f32, h as f32);
                        self.thumbnails.insert(key, ThumbnailState::Loaded(tex, orig_size));
                    }
                    Err(e) => {
                        self.thumbnails.insert(key, ThumbnailState::Failed);
                        self.messages.push(format!("Thumbnail load failed: {}", e));
                    }
                }
            }
        }

        match self.loading_phase {
            LoadingPhase::AddingFiles => {
                if self.loading_phase == LoadingPhase::AddingFiles {
                    let mut finished = false;
                    if let Some(rx) = self.loader_rx.take() {
                        use std::sync::mpsc::TryRecvError;
                        loop {
                            match rx.try_recv() {
                                Ok(path) => {
                                    self.add_files(vec![path]);
                                    self.loading_count += 1;
                                }
                                Err(TryRecvError::Empty) => {
                                    self.loader_rx = Some(rx);
                                    break;
                                }
                                Err(TryRecvError::Disconnected) => {
                                    finished = true;
                                    break;
                                }
                            }
                        }
                    }
                    if finished {
                        self.loading_phase = LoadingPhase::LoadingThumbs;
                        self.loading_count = 0;
                    }
                    ctx.request_repaint();
                }
                   
            }
            LoadingPhase::LoadingThumbs => {
                // Check if any thumbnails are still loading
                let any_loading = self.thumbnails.values().any(|s| matches!(s, ThumbnailState::Loading));
                if !any_loading {
                    self.loading_phase = LoadingPhase::None;
                }
                ctx.request_repaint();
            }
            LoadingPhase::None => {}
        }

        let dropped = ctx.input_mut(|i| {
            if i.raw.dropped_files.is_empty() {
                None
            } else {
                Some(std::mem::take(&mut i.raw.dropped_files))
            }
        });

        if let Some(dropped_files) = dropped {
            let (tx, rx) = mpsc::channel::<PathBuf>();
            self.loader_rx = Some(rx);
            self.loading_phase = LoadingPhase::AddingFiles;
            thread::spawn(move || {
                let mut collected = Vec::new();
                for f in dropped_files {
                    if let Some(path) = f.path {
                        if path.is_file() {
                            collected.push(path);
                        } else if path.is_dir() {
                            Self::collect_files_recursively(&path, &mut collected);
                        }
                    }
                }
                for path in collected {
                    tx.send(path).ok();
                }
            });
            ctx.request_repaint();
        }

        if self.loading_phase == LoadingPhase::None
            && ctx.input(|i| !i.raw.hovered_files.is_empty())
        {
            egui::Area::new("drop_overlay".into())
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    let rect = ctx.available_rect();
                    ui.painter().rect_filled(
                        rect,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(50, 100, 200, 80),
                    );
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Drop files to add")
                                .size(32.0)
                                .color(egui::Color32::WHITE),
                        );
                    });
                }
            );
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("BulkReName v2");

            // top buttons
            ui.horizontal(|ui| {
                if ui.button("Add files...").clicked() {
                    if let Some(paths) = rfd::FileDialog::new().pick_files() {
                        self.add_files(paths);
                    }
                }
                if ui.button("Clear files").clicked() {
                    self.files.clear();
                    self.selected_idx = None;
                }
                if ui.button("ReName").clicked() {
                    self.execute_rename();
                }
                if ui.button("Undo").clicked() {
                    self.undo();
                }
            });

            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Sort: ");
                let namelabel = match (self.sort_key, self.sort_order) {
                    (Some(SortKey::Name), SortOrder::Asc) => "Name ↓",
                    (Some(SortKey::Name), SortOrder::Desc) => "Name ↑",
                    _ => "Name",
                };
                let modifiedlabel = match (self.sort_key, self.sort_order) {
                    (Some(SortKey::Modified), SortOrder::Asc) => "Modified ↓",
                    (Some(SortKey::Modified), SortOrder::Desc) => "Modified ↑",
                    _ => "Modified",
                };
                let createdlabel = match (self.sort_key, self.sort_order) {
                    (Some(SortKey::Created), SortOrder::Asc) => "Created ↓",
                    (Some(SortKey::Created), SortOrder::Desc) => "Created ↑",
                    _ => "Created",
                };
                let sizelabel = match (self.sort_key, self.sort_order) {
                    (Some(SortKey::Size), SortOrder::Asc) => "Size ↓",
                    (Some(SortKey::Size), SortOrder::Desc) => "Size ↑",
                    _ => "Size",
                };
                if ui.button(namelabel).clicked() {
                    self.sort_files(SortKey::Name);
                }
                if ui.button(modifiedlabel).clicked() {
                    self.sort_files(SortKey::Modified);
                }
                if ui.button(createdlabel).clicked() {
                    self.sort_files(SortKey::Created);
                }
                if ui.button(sizelabel).clicked() {
                    self.sort_files(SortKey::Size);
                }
            });

            ui.separator();

            ui.columns(2, |cols| {
                // Left panel: file list
                let left = &mut cols[0];
                left.label(RichText::new("Files (select then move)").strong());
                left.checkbox(&mut self.show_thumbnails, "show thumbnail");

                egui::ScrollArea::vertical()
                    .max_height(800.0)
                    .auto_shrink([false, false])
                    .id_source("file_list")
                    .show(left, |ui| {
                        let mut to_delete = None;
                        let pointer_y = ui.input(|i| i.pointer.hover_pos().map(|p| p.y));
                        // rect
                        let mut item_rects = Vec::new();
                        for _ in 0..self.files.len() {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 60.0),
                                egui::Sense::click_and_drag(),
                            );
                            item_rects.push(rect);
                        }
                        // Insertion position calculation
                        let mut insert_index = None;
                        if let Some(py) = pointer_y {
                            for (i, rect) in item_rects.iter().enumerate() {
                                if py < rect.center().y {
                                    insert_index = Some(i);
                                    break;
                                }
                            }
                            if insert_index.is_none() {
                                insert_index = Some(self.files.len());
                            }
                        }
                        // drow
                        for i in 0..self.files.len() {
                            let rect = item_rects[i];
                            let id = ui.id().with(i);
                            let resp = ui.interact(rect, id, egui::Sense::click_and_drag());

                            // start drag
                            if resp.drag_started() {
                                self.dragging_idx = Some(i);
                                self.selected_idx = Some(i);
                            }
                            if resp.clicked() {
                                self.selected_idx = Some(i);
                            }

                            // item background
                            if Some(i) == self.dragging_idx {
                                ui.painter().rect_filled(rect, 6.0, egui::Color32::DARK_GRAY);
                            } else if Some(i) == self.selected_idx {
                                ui.painter().rect_filled(rect, 6.0, egui::Color32::from_rgb(180, 180, 180));
                            }
                            if ui.input(|i| i.pointer.any_pressed()) && self.dragging_idx.is_none() {
                                self.selected_idx = None;
                            }

                            // contents
                            ui.allocate_ui_at_rect(rect, |ui| {
                                ui.horizontal(|ui| {
                                    // delete buttom
                                    //ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                                        let del_btn = egui::Button::new("×")
                                            .fill(egui::Color32::from_rgb(240, 120, 120));

                                        if ui.add(del_btn).clicked() {
                                            to_delete = Some(i);
                                        }
                                    //});
                                    ui.separator();
                                    // ▲▼
                                    ui.vertical(|ui| {
                                        if ui.small_button("▲").clicked() {
                                            self.selected_idx = Some(i);
                                            self.move_up();
                                        }
                                        if ui.small_button("▼").clicked() {
                                            self.selected_idx = Some(i);
                                            self.move_down();
                                        }
                                    });

                                    // サムネ
                                    if self.show_thumbnails {
                                        let path_buf = self.files[i].path.clone();
                                        let key = path_buf.to_string_lossy().to_string();

                                        if !self.thumbnails.contains_key(&key) {
                                            self.ensure_thumbnail(ui.ctx(), &path_buf);
                                        }

                                        match self.thumbnails.get(&key) {
                                            Some(ThumbnailState::Loaded(tex, orig_size)) => {
                                                let max_w = self.thumb_max_size.0 as f32;
                                                let max_h = self.thumb_max_size.1 as f32;
                                                let scale = (max_w / orig_size.x)
                                                    .min(max_h / orig_size.y)
                                                    .min(1.0);
                                                let size = *orig_size * scale;
                                                ui.image((tex.id(), size));
                                            }
                                            Some(ThumbnailState::Loading) => {
                                                ui.spinner();
                                            }
                                            _ => {}
                                        }
                                    }

                                    // file name
                                    let full = self.files[i]
                                        .path
                                        .file_name()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("")
                                        .to_string();

                                    let disp = {
                                        let chars: Vec<char> = full.chars().collect();
                                        if chars.len() > 20 {
                                            let first: String = chars[..10].iter().collect();
                                            let last: String = chars[chars.len() - 9..].iter().collect();
                                            format!("{}…{}", first, last)
                                        } else {
                                            full.clone()
                                        }
                                    };

                                    ui.label(disp).on_hover_text(full);

                                });
                            });
                            // sparator
                            ui.painter().line_segment(
                                [
                                    egui::pos2(rect.left(), rect.bottom()),
                                    egui::pos2(rect.right(), rect.bottom()),
                                ],
                                egui::Stroke::new(1.0, egui::Color32::from_gray(80)),
                            );
                        }
                        // when drop reorder
                        if let Some(drag_i) = self.dragging_idx {
                            if ui.input(|i| i.pointer.any_released()) {
                                if let Some(target) = insert_index {
                                    let item = self.files.remove(drag_i);
                                    let mut target = target;
                                    if drag_i < target { target -= 1; }
                                    let target = target.min(self.files.len());
                                    self.files.insert(target, item);
                                    self.selected_idx = Some(target);
                                }
                                self.dragging_idx = None;
                            }
                        }

                        // dropline
                        if self.dragging_idx.is_some() {
                            if let Some(target) = insert_index {
                                if !item_rects.is_empty() {
                                    let y = if target < item_rects.len() {
                                        item_rects[target].top()
                                    } else {
                                        item_rects.last().unwrap().bottom()
                                    };
                                    let base = if target < item_rects.len() {
                                        item_rects[target]
                                    } else {
                                        *item_rects.last().unwrap()
                                    };

                                    ui.painter().line_segment(
                                        [
                                            egui::pos2(base.left(), y),
                                            egui::pos2(base.right(), y),
                                        ],
                                        egui::Stroke::new(3.0, egui::Color32::LIGHT_BLUE),
                                    );
                                }
                            }
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                        }

                        // delete
                        if self.dragging_idx.is_none() {
                            if let Some(i) = to_delete {
                                if i < self.files.len() {
                                    self.selected_idx = Some(i);
                                    self.remove_selected();
                                }
                            }
                        }
                    });
                

                // Right panel: template, preview, persistence
                let right = &mut cols[1];
                right.label(RichText::new("Template Blocks").strong());

                // blocks editor ...
                let mut idx = 0;
                while idx < self.blocks.len() {
                    let blk = self.blocks[idx].clone();
                    let mut new_blk = blk.clone();
                    let mut action: Option<&str> = None;
                    right.horizontal(|ui| {
                        let del_block = egui::Button::new("×")
                            .fill(egui::Color32::from_rgb(240, 150, 150));
                        if ui.add(del_block).clicked() {
                            if self.blocks.len() <= 1 {
                                self.show_delete_error = true;
                            } else {
                                action = Some("del");
                            }
                        }
                        ui.separator();
                        if ui.small_button("▲").clicked() && idx > 0 {
                            action = Some("up");
                        }
                        if ui.small_button("▼").clicked() && idx + 1 < self.blocks.len() {
                            action = Some("down");
                        }
                        ui.label(format!("[{}]", idx));
                        match &mut new_blk {
                            Block::Literal(s) => {
                                ui.label("<Literal>");
                                ui.text_edit_singleline(s);
                            }
                            Block::Number { width, start, step } => {
                                ui.label("<Number>min digits:");
                                ui.add(DragValue::new(width).clamp_range(0..=20));
                                ui.label("init:");
                                ui.add(DragValue::new(start));
                                ui.label("gain:");
                                ui.add(DragValue::new(step));
                            }
                            Block::Date { format } => {
                                ui.label("<Date fmt>");
                                ui.text_edit_singleline(format);
                                ui.label("(strftime)");
                            }
                            Block::Original { mode } => {
                                ui.label("<Orig>");

                                egui::ComboBox::from_id_source(format!("orig_mode_{}", idx))
                                    .selected_text(match mode {
                                        OriginalMode::Keep => "Keep",
                                        OriginalMode::RemoveRange { .. } => "Del Range",
                                        OriginalMode::RemoveSubstring { .. } => "Del Substr.",
                                    })
                                    .show_ui(ui, |ui| {
                                        if ui.selectable_label(matches!(mode, OriginalMode::Keep), "Keep").clicked() {
                                            *mode = OriginalMode::Keep;
                                        }
                                        if ui.selectable_label(matches!(mode, OriginalMode::RemoveRange { .. }), "Range").clicked() {
                                            *mode = OriginalMode::RemoveRange { start: 0, end: 1 };
                                        }
                                        if ui.selectable_label(matches!(mode, OriginalMode::RemoveSubstring { .. }), "Substring").clicked() {
                                            *mode = OriginalMode::RemoveSubstring { pattern: "".into(), case_sensitive: true, };
                                        }
                                    });

                                match mode {
                                    OriginalMode::RemoveRange { start, end } => {
                                        ui.label("range:");
                                        ui.add(DragValue::new(start));
                                        ui.label("～");
                                        ui.add(DragValue::new(end));
                                    }
                                    OriginalMode::RemoveSubstring { pattern, case_sensitive } => {
                                        ui.vertical(|ui| {
                                            ui.horizontal(|ui| {
                                                ui.toggle_value(case_sensitive, "Aa")
                                                    .on_hover_text("Case sensitive");
                                                ui.label("pattern:");
                                                ui.text_edit_singleline(pattern);
                                            });
                                        });
                                    }
                                    _ => {}
                                }
                            }
                            Block::Extension => {
                                ui.label("<Extension>");
                            }
                        }
                        if self.show_delete_error {
                            egui::Window::new("ERROR")
                                .collapsible(false)
                                .resizable(false)
                                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                                .fixed_size(egui::vec2(180.0, 90.0))
                                .show(ui.ctx(), |ui| {
                                    ui.vertical_centered(|ui| {
                                        ui.set_width(160.0);

                                        ui.add(
                                            egui::Label::new("need at least 1 block")
                                                .wrap(true)
                                        );

                                        ui.add_space(8.0);

                                        if ui.button("OK").clicked() {
                                            self.show_delete_error = false;
                                        }
                                    });
                                });
                        }
                    });
                    if let Some(act) = action {
                        match act {
                            "up" => {
                                self.blocks.swap(idx, idx - 1);
                                idx -= 1;
                                continue;
                            }
                            "down" => {
                                self.blocks.swap(idx, idx + 1);
                                idx += 1;
                                continue;
                            }
                            "del" => {
                                self.blocks.remove(idx);
                                continue;
                            }
                            _ => {}
                        }
                    }
                    self.blocks[idx] = new_blk;
                    idx += 1;
                }

                right.horizontal(|ui| {
                    if ui.button("Add Literal").clicked() {
                        self.blocks.push(Block::Literal(String::new()));
                    }
                    if ui.button("Add Number").clicked() {
                        self.blocks.push(Block::Number {
                            width: 4,
                            start: 1,
                            step: 1,
                        });
                    }
                    if ui.button("Add Date").clicked() {
                        self.blocks.push(Block::Date {
                            format: "%Y%m%d".into(),
                        });
                    }
                    if ui.button("Add Original").clicked() {
                        self.blocks.push(Block::Original { mode: OriginalMode::Keep } );
                    }
                    if ui.button("Add Extension").clicked() {
                        self.blocks.push(Block::Extension);
                    }
                });
                right.separator();

                right.label("Collision strategy:");
                right.horizontal(|ui| {
                    ui.radio_value(&mut self.collision, CollisionStrategy::Overwrite, "Overwrite");
                    ui.radio_value(&mut self.collision, CollisionStrategy::Skip, "Skip");
                    ui.radio_value(&mut self.collision, CollisionStrategy::Suffix, "Suffix (1)");
                });
                //right.checkbox(&mut self.use_mtime_for_date, "Use file mtime for date");

                right.separator();
                right.label(RichText::new("Preview").strong());
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([false, false])
                    .id_source("preview")
                    .show(right, |ui| {
                        let w = ui.available_width();
                        for (old, new_name) in self.preview_table().iter() {
                            let txt = {
                                let chars: Vec<char> = old.chars().collect();
                                if chars.len() > 20 {
                                    let first_10: String = chars[..10].iter().collect();
                                    let last_9: String = chars[chars.len().saturating_sub(9)..].iter().collect();
                                    format!("{}…{}", first_10, last_9)
                                } else {
                                    old.clone()
                                }
                            };
                            let lbl = ui.label(txt);
                            lbl.on_hover_text(old);

                            ui.horizontal(|ui| {
                                ui.label("→");
                                ui.add_sized(
                                    [w * 0.8, 0.0],
                                    egui::Label::new(
                                        RichText::new(new_name.clone())
                                            .color(egui::Color32::BLUE),
                                    )
                                    .wrap(true),
                                );
                            });
                            ui.separator();
                        }
                    });

                // Persist template UI
                right.separator();
                right.label(RichText::new("Save / Load Template").strong());
                right.horizontal(|ui| {
                    ui.label("Name:");
                    ui.text_edit_singleline(&mut self.current_template_name);
                    if ui.button("Save").clicked() && !self.current_template_name.is_empty() {
                        let tpl = Template {
                            name: self.current_template_name.clone(),
                            blocks: self.blocks.clone(),
                            collision: self.collision,
                            use_mtime_for_date: self.use_mtime_for_date,
                        };
                        if let Some(pos) = self
                            .saved_templates
                            .iter()
                            .position(|t| t.name == tpl.name)
                        {
                            self.saved_templates[pos] = tpl;
                        } else {
                            self.saved_templates.push(tpl);
                        }
                        self.save_templates();
                    }
                });
                right.horizontal(|ui| {
                    ui.label("Load:");
                    ComboBox::from_id_source("template_load")
                        .selected_text(&self.current_template_name)
                        .show_ui(ui, |ui| {
                            for tpl in &self.saved_templates {
                                ui.selectable_value(
                                    &mut self.current_template_name,
                                    tpl.name.clone(),
                                    &tpl.name,
                                );
                            }
                        });
                    if ui.button("Apply").clicked() {
                        if let Some(tpl) = self
                            .saved_templates
                            .iter()
                            .find(|t| t.name == self.current_template_name)
                        {
                            self.blocks = tpl.blocks.clone();
                            self.collision = tpl.collision;
                            self.use_mtime_for_date = tpl.use_mtime_for_date;
                        }
                    }
                    if ui.button("Delete").clicked() {
                        if let Some(pos) = self
                            .saved_templates
                            .iter()
                            .position(|t| t.name == self.current_template_name)
                        {
                            self.saved_templates.remove(pos);
                            self.current_template_name.clear();
                            self.save_templates();
                            self.messages.push("Template deleted.".into());
                        }
                    }
                });
            });

            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(120.0)
                .show(ui, |ui| {
                    for m in &self.messages {
                        ui.label(m);
                    }
                });
        });

        if self.loading_phase == LoadingPhase::AddingFiles || self.loading_phase == LoadingPhase::LoadingThumbs {
            ctx.request_repaint();
        }
    }
}

fn main() {
    let bytes = include_bytes!("../BulkReName.png");
    let img = image::load_from_memory(bytes).expect("Failed to load icon");
    let (w, h) = img.dimensions();
    let rgba = img.to_rgba8().into_raw();
    let viewport = ViewportBuilder::default()
        .with_inner_size([1000.0, 800.0])
        .with_icon(egui::IconData {
            rgba,
            width: w,
            height: h,
        })
        .with_min_inner_size(egui::Vec2::new(300.0, 250.0));
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let result = eframe::run_native(
        "BulkReName",
        options,
        Box::new(|cc| {
            // Optional: embed Japanese font
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "noto_jp".to_owned(),
                egui::FontData::from_static(include_bytes!("../NotoSansJP-Regular.ttf")),
            );
            use egui::FontFamily;
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, "noto_jp".to_owned());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .insert(0, "noto_jp".to_owned());
            cc.egui_ctx.set_fonts(fonts);

            let mut app = BulkRename::default();
            app.load_templates();
            Box::new(app)
        }),
    );

    if let Err(e) = result {
        eprintln!("eframe run_native failed: {}", e);
        std::process::exit(1);
    }
}
