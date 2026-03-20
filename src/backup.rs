// BulkReName GUI using eframe (egui) + rfd for file dialog.

#![windows_subsystem = "windows"]

use chrono::{DateTime, Local};
use directories::ProjectDirs;
use eframe::{egui, egui::RichText};
use egui::{ComboBox, DragValue, Layout};
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
    Original,
    Extension,
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

struct RenamerApp {
    files: Vec<FileEntry>,
    selected_idx: Option<usize>,
    blocks: Vec<Block>,
    collision: CollisionStrategy,
    use_mtime_for_date: bool,
    last_actions: Vec<HashMap<PathBuf, PathBuf>>,
    messages: Vec<String>,
    // thumbnail cache: key = path → state
    thumbnails: HashMap<String, ThumbnailState>,
    thumb_max_size: (usize, usize),
    thumb_tx: Option<SyncSender<(String, Result<(image::RgbaImage, (usize, usize)), String>)>>,
    thumb_rx: Option<Receiver<(String, Result<(image::RgbaImage, (usize, usize)), String>)>>,
    // persistence
    saved_templates: Vec<Template>,
    current_template_name: String,
    //loading
    loading_phase: LoadingPhase,
    loader_rx: Option<Receiver<PathBuf>>,
    loading_count: usize,
}

impl Default for RenamerApp {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            selected_idx: None,
            blocks: vec![
                Block::Original,
                Block::Extension,
            ],
            collision: CollisionStrategy::Suffix,
            use_mtime_for_date: true,
            last_actions: Vec::new(),
            messages: Vec::new(),
            thumbnails: HashMap::new(),
            thumb_max_size: (160, 120),
            thumb_tx: None,
            thumb_rx: None,
            saved_templates: Vec::new(),
            current_template_name: String::new(),
            //loading
            loading_phase: LoadingPhase::None,
            loader_rx: None,
            loading_count: 0,
        }
    }
}

impl RenamerApp {
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
                    Block::Original => parts.push(file_name.clone()),
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

    fn execute_rename(&mut self) {
        let targets = self.generate_targets();
        if targets.len() != self.files.len() {
            return;
        }

        let mut final_paths = Vec::new();
        for (fe, tname) in self.files.iter().zip(targets.iter()) {
            let mut p = fe.path.clone();
            p.set_file_name(tname);
            final_paths.push(p);
        }

        // Build robust map orig -> (tmp, final)
        let mut robust_map = Vec::new();
        for (i, fe) in self.files.iter().enumerate() {
            let orig = fe.path.clone();
            let dir = orig.parent().unwrap_or(Path::new("."));
            let mut desired = final_paths[i].clone();
            if desired.exists() {
                match self.collision {
                    CollisionStrategy::Overwrite => {}
                    CollisionStrategy::Skip => {
                        desired = orig.clone();
                    }
                    CollisionStrategy::Suffix => {
                        let mut n = 1;
                        loop {
                            let candidate = append_suffix_before_ext(
                                &desired,
                                format!(" ({})", n).as_str(),
                            );
                            if !candidate.exists() {
                                desired = candidate;
                                break;
                            }
                            n += 1;
                        }
                    }
                }
            }
            if desired == orig {
                continue;
            }
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let tmp_name = format!(".tmp-{}-{}", nanos, i);
            let mut tmp_path = dir.join(&tmp_name);
            tmp_path.set_extension("tmp");
            robust_map.push((orig, tmp_path, desired));
        }

        if robust_map.is_empty() {
            self.messages
                .push("No files to rename (all skipped or no files).".into());
            return;
        }

        // Step A: orig -> tmp
        let mut temps_created = Vec::new();
        for (orig, tmp, _) in robust_map.iter() {
            if let Err(e) = fs::rename(orig, tmp) {
                self.messages
                    .push(format!("Failed to move {:?} -> {:?}: {}", orig, tmp, e));
                for (t, o) in temps_created.iter().rev() {
                    let _ = fs::rename(t, o);
                }
                self.messages.push("Performed rollback after failure.".into());
                return;
            }
            temps_created.push((tmp.clone(), orig.clone()));
        }

        // Step B: tmp -> final
        let final_mappings: HashMap<PathBuf, PathBuf> = HashMap::new(); // explicit types
        for (_orig, tmp, final_path) in robust_map.iter() {
            if let Err(e) = fs::rename(tmp, final_path) {
                self.messages.push(format!(
                    "Failed to move temp {:?} -> final {:?}: {}",
                    tmp, final_path, e
                ));
                for (t, o) in &temps_created {
                    if t.exists() {
                        let _ = fs::rename(t, o);
                    }
                }
                for (o, f) in &final_mappings {
                    if f.exists() {
                        let _ = fs::rename(f, o);
                    }
                }
                self.messages.push("Attempted rollback after partial failure.".into());
                return;
            }
        }

        // Build undo map orig -> final
        let mut undo_map: HashMap<PathBuf, PathBuf> = HashMap::new();
        for (orig, _tmp, final_path) in robust_map {
            undo_map.insert(orig, final_path);
        }
        self.last_actions.push(undo_map);
        self.messages.push("Rename completed successfully.".into());
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

fn append_suffix_before_ext(p: &PathBuf, suffix: &str) -> PathBuf {
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ext = p.extension().and_then(|s| s.to_str());
    let dir = p.parent().unwrap_or(Path::new("."));
    let new_stem = format!("{}{}", stem, suffix);
    if let Some(e) = ext {
        dir.join(format!("{}.{}", new_stem, e))
    } else {
        dir.join(new_stem)
    }
}

impl eframe::App for RenamerApp {
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
                        if !paths.is_empty() {
                            let (tx, rx) = mpsc::channel::<PathBuf>();
                            self.loader_rx = Some(rx);
                            self.loading_phase = LoadingPhase::AddingFiles;
                            thread::spawn(move || {
                                for p in paths {
                                    tx.send(p).ok();
                                }
                            });
                            ctx.request_repaint();
                        }
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
                if self.loading_phase == LoadingPhase::AddingFiles {
                    ui.separator();
                    ui.spinner();
                    ui.label(format!(
                        "Loading files... ({})",
                        self.loading_count
                    ));
                } else if self.loading_phase == LoadingPhase::LoadingThumbs {
                    let loading_thumb_count = self.thumbnails.values().filter(|s| matches!(s, ThumbnailState::Loading)).count();
                    ui.separator();
                    ui.spinner();
                    ui.label(format!(
                        "Loading thumbnails... ({})",
                        loading_thumb_count
                    ));
                }
            });

            ui.separator();

            ui.columns(2, |cols| {
                // Left panel: file list
                let left = &mut cols[0];
                left.label(RichText::new("Files (select then move)").strong());
                egui::ScrollArea::vertical()
                    .max_height(800.0)
                    .auto_shrink([false, false])
                    .id_source("file_list")
                    .show(left, |ui| {
                        let mut to_delete = None;

                        for i in 0..self.files.len() {
                            let full = self.files[i]
                                .path
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or("")
                                .to_string();
                            let disp = {
                                let chars: Vec<char> = full.chars().collect();
                                if chars.len() > 20 {
                                    let first_10: String = chars[..10].iter().collect();
                                    let last_9: String = chars[chars.len().saturating_sub(9)..].iter().collect();
                                    format!("{}…{}", first_10, last_9)
                                } else {
                                    full.clone()
                                }
                            };
                            ui.horizontal(|ui| {
                                ui.with_layout(Layout::left_to_right(egui::Align::TOP), |ui| {
                                    ui.horizontal(|ui| {
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

                                        // thumbnail
                                        let path_buf = self.files[i].path.clone();
                                        let key = path_buf.to_string_lossy().to_string();
                                        if !self.thumbnails.contains_key(&key) {
                                            self.ensure_thumbnail(ctx, &path_buf);
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

                                        let selected = Some(i) == self.selected_idx;
                                        let resp = ui.selectable_label(selected, disp);
                                        resp.on_hover_text(full);
                                    });
                                });

                                ui.with_layout(Layout::right_to_left(egui::Align::TOP), |ui| {
                                    ui.add_space(10.0);
                                    let del_btn = egui::Button::new("Del")
                                        .fill(egui::Color32::from_rgb(240, 150, 150));

                                    if ui.add(del_btn).clicked() {
                                        to_delete = Some(i);
                                        self.selected_idx = Some(i);
                                    }
                                });
                            });

                            ui.separator();
                        }
                        if let Some(i) = to_delete {
                            self.selected_idx = Some(i);
                            self.remove_selected();
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
                            Block::Original => {
                                ui.label("<Orig. Name>");
                            }
                            Block::Extension => {
                                ui.label("<Extension>");
                            }
                        }
                        if ui.small_button("Del").clicked() {
                            action = Some("del");
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
                        self.blocks.push(Block::Original);
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
                right.checkbox(&mut self.use_mtime_for_date, "Use file mtime for date");

                right.separator();
                right.label(RichText::new("Preview").strong());
                egui::ScrollArea::vertical()
                    .max_height(200.0)
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

            let mut app = RenamerApp::default();
            app.load_templates();
            Box::new(app)
        }),
    );

    if let Err(e) = result {
        eprintln!("eframe run_native failed: {}", e);
        std::process::exit(1);
    }
}
