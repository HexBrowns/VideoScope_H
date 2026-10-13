//! ビデオスコープのウィンドウ（egui）
//!
//! 上: 有効にする・種類・強度・間引き・限定範囲の線。中: スコープの絵（目盛りの数字は egui で重ねる）。下: 状態の行。
//! 見えていない間・有効にしない間は、本体に何も頼まない。

use std::sync::Arc;
use std::time::Duration;

use aviutl2_eframe::{eframe, egui, AviUtl2EframeHandle};

use crate::request::{self, Status};
use crate::scope::{self, Kind, Options};
use crate::worker::{Shared, Source};

const RED: egui::Color32 = egui::Color32::from_rgb(235, 110, 110);
const DIM: egui::Color32 = egui::Color32::from_rgb(150, 150, 150);
const HIDDEN_CHECK: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(100);

/// しきい値（0〜100%）の欄。値を使うのは Enter か、ほかをクリックしたとき。入力中の Esc で打つ前の値に戻す
/// （ルール au2-rs-plugin「入力の確定と取り消し」）。
/// egui 0.36.2 の `DragValue` は `update_while_editing(false)` でも、Esc の次のフレームで打った文字を値にしてしまう
/// （フォーカスを失ったとみなす期間が 2 フレームあり、2 フレーム目には Esc が押されていないため）。
/// Esc のフレームの値を覚えておき、次のフレームで戻す（LayerSilenceCut_H の `setting_value` と同じ）
fn percent_value(ui: &mut egui::Ui, value: &mut f32) -> egui::Response {
    let mut resp = ui.add(
        egui::DragValue::new(value)
            .range(0.0..=100.0)
            .speed(0.1)
            .fixed_decimals(1)
            .suffix("%")
            .update_while_editing(false),
    );
    let key = resp.id.with("escaped");
    let pass = ui.ctx().cumulative_pass_nr();
    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        ui.data_mut(|d| d.insert_temp(key, (pass, *value)));
    } else if let Some((at, before)) = ui.data_mut(|d| d.remove_temp::<(u64, f32)>(key)) {
        if pass == at + 1 {
            if *value == before {
                resp.flags.remove(egui::response::Flags::CHANGED);
            }
            *value = before;
        }
    }
    resp
}

pub struct ScopeApp {
    _handle: AviUtl2EframeHandle,
    /// 本体のフォント（窓が見えてから読む。`host_fonts`）
    host_fonts: crate::host_fonts::LazyFonts,
    shared: Arc<Shared>,
    enabled: bool,
    opts: Options,
    step: usize,
    texture: Option<egui::TextureHandle>,
    kind_shown: Kind,
    frame: Option<u32>,
    render_ms: f32,
    source: Source,
    calc_ms: f32,
    sampled: (usize, usize),
    status: Status,
}

impl ScopeApp {
    pub fn new(cc: &eframe::CreationContext<'_>, handle: AviUtl2EframeHandle, shared: Arc<Shared>) -> Self {
        cc.egui_ctx.all_styles_mut(|style| {
            style.visuals = aviutl2_eframe::aviutl2_visuals();
        });
        // 本体のフォントは窓が見えてから ui で読む（ここで読むと、本体の起動がこの窓の初期化を待つ間ずっと止まる。`host_fonts`）
        let host_fonts = crate::host_fonts::LazyFonts::new(&handle);
        let _ = crate::EGUI_CTX.set(cc.egui_ctx.clone());
        let opts = Options::default();
        shared.set_options(opts, 2);
        ScopeApp {
            _handle: handle,
            host_fonts,
            shared,
            enabled: true,
            opts,
            step: 2,
            texture: None,
            kind_shown: opts.kind,
            frame: None,
            render_ms: 0.0,
            source: Source::Render,
            calc_ms: 0.0,
            sampled: (0, 0),
            status: Status::Waiting,
        }
    }

    fn render_top(&mut self, ui: &mut egui::Ui) {
        let before = (self.opts, self.step, self.enabled);
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.enabled, "有効にする").on_hover_text("オフの間は本体に描画を頼まない（重いシーンで切る）");
            ui.label("種類:");
            egui::ComboBox::from_id_salt("kind").selected_text(self.opts.kind.label()).show_ui(ui, |ui| {
                for k in Kind::ALL {
                    ui.selectable_value(&mut self.opts.kind, k, k.label());
                }
            });
            ui.label("強度:");
            ui.add(egui::Slider::new(&mut self.opts.intensity, 0.1..=10.0).logarithmic(true).fixed_decimals(2));
        });
        ui.horizontal(|ui| {
            ui.label("間引き:");
            for (s, label) in [(1usize, "等倍"), (2, "1/2"), (4, "1/4")] {
                ui.radio_value(&mut self.step, s, label);
            }
            if self.opts.kind != Kind::Vector {
                ui.checkbox(&mut self.opts.legal_lines, "16 / 235 の線").on_hover_text("限定範囲（16〜235）の目安の線");
                ui.checkbox(&mut self.opts.warn_lines, "しきい値の線")
                    .on_hover_text("レンジ警告_H の白飛びしきい値・黒潰れしきい値の線（既定は 99% と 1%）");
                if self.opts.warn_lines {
                    ui.label("白飛び");
                    percent_value(ui, &mut self.opts.warn_high);
                    ui.label("黒潰れ");
                    percent_value(ui, &mut self.opts.warn_low);
                }
            }
        });
        if (self.opts, self.step, self.enabled) != before {
            self.shared.set_options(self.opts, self.step);
            if self.enabled && !before.2 {
                request::mark_dirty();
            }
        }
    }

    fn take_output(&mut self, ctx: &egui::Context) {
        if let Some(o) = self.shared.take_output() {
            let img = egui::ColorImage::from_rgba_unmultiplied([o.image.width, o.image.height], &o.image.rgba);
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::NEAREST),
                None => self.texture = Some(ctx.load_texture("VideoScope_H", img, egui::TextureOptions::NEAREST)),
            }
            self.kind_shown = o.image.kind;
            self.frame = Some(o.frame);
            if o.render_ms >= 0.0 {
                self.render_ms = o.render_ms;
                self.source = o.source;
            }
            self.calc_ms = o.calc_ms;
            self.sampled = o.sampled;
        }
    }

    fn render_scope(&self, ui: &mut egui::Ui) {
        let Some(tex) = &self.texture else {
            ui.colored_label(DIM, "まだ描いていません");
            return;
        };
        let size = tex.size_vec2();
        let avail = ui.available_size();
        let scale = (avail.x / size.x).min(avail.y / size.y).max(0.1);
        let draw = size * scale;
        let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
        let img_rect = egui::Rect::from_center_size(rect.center(), draw);
        let painter = ui.painter_at(rect);
        painter.image(tex.id(), img_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
        let font = egui::FontId::proportional(11.0);
        if self.kind_shown == Kind::Vector {
            // 目標の名前（100% の枠の外側）
            for (name, rgb) in scope::bar_targets(1.0) {
                let (cb, cr) = scope::cbcr(rgb[0], rgb[1], rgb[2]);
                let (x, y) = scope::vec_xy(cb * 1.12, cr * 1.12);
                let p = img_rect.min + egui::vec2(x as f32, y as f32) * scale;
                painter.text(p, egui::Align2::CENTER_CENTER, name, font.clone(), DIM);
            }
        } else if self.kind_shown == Kind::Histogram {
            // 横軸の値（下の余白に）
            for pct in [0, 25, 50, 75, 100] {
                let x = (scope::hist_x(pct as f32 / 100.0) as f32 + 0.5).min(scope::HIST_W as f32 - 8.0).max(8.0);
                let p = egui::pos2(img_rect.min.x + x * scale, img_rect.max.y);
                painter.text(p, egui::Align2::CENTER_BOTTOM, format!("{pct}"), font.clone(), DIM);
            }
        } else {
            for pct in [0, 25, 50, 75, 100] {
                let y = scope::wave_row(pct as f32 / 100.0) as f32 + 0.5;
                let p = egui::pos2(img_rect.min.x + 2.0, img_rect.min.y + y * scale);
                painter.text(p, egui::Align2::LEFT_BOTTOM, format!("{pct}"), font.clone(), DIM);
            }
        }
    }

    fn render_bottom(&self, ui: &mut egui::Ui) {
        let frame = self.frame.map(|f| format!("フレーム {f}")).unwrap_or_else(|| "フレーム -".into());
        let state = match &self.status {
            Status::PlayingLive { empty: false } => "再生中（受け口から）".to_string(),
            Status::PlayingLive { empty: true } => {
                "受け口から空の画面が届いています。受け口をカメラ制御の対象から外してください".to_string()
            }
            Status::PlayingWaiting => "再生中（受け口を待っています）".to_string(),
            Status::PlayingNoReceiver => {
                "再生中は FullSizePreview_H v0.6.0 以降の「フルサイズプレビュー受け口」が要ります（編集メニューの「フルサイズプレビュー受け口を置く」）"
                    .to_string()
            }
            Status::PlayingNoShare => "再生中の受け渡しの準備に失敗しました（ログを見てください）".to_string(),
            Status::Saving => "出力中は更新しません".to_string(),
            Status::Requested => "描画中".to_string(),
            Status::Waiting => "停止中".to_string(),
            Status::Failed(e) => e.clone(),
        };
        let got = match self.source {
            Source::Render => "本体の描画",
            Source::Receiver => "受け口の読み出し",
        };
        let text = format!(
            "{frame}（{state}）/ {got} {:.1}ms / 集計 {:.1}ms / 測った画素 {}×{}",
            self.render_ms, self.calc_ms, self.sampled.0, self.sampled.1
        );
        let warn = matches!(
            self.status,
            Status::Failed(_) | Status::PlayingLive { empty: true } | Status::PlayingNoReceiver | Status::PlayingNoShare
        );
        if warn {
            ui.colored_label(RED, egui::RichText::new(text).small());
        } else {
            ui.label(egui::RichText::new(text).small());
        }
    }
}

impl eframe::App for ScopeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 本体のフォントは窓が見えてから読む。読んだフレームは描かない（新しいフォントは次のフレームから効く。次のフレームは頼んである）
        if self.host_fonts.load_if_visible(ui.ctx()) {
            return;
        }
        // 本体のパネルに出ていない間は頼まない（印は残り、出たときに頼む）
        if !crate::window_visible() {
            ui.ctx().request_repaint_after(HIDDEN_CHECK);
            return;
        }
        self.take_output(ui.ctx());
        if self.enabled {
            self.status = request::poll(&self.shared);
            ui.ctx().request_repaint_after(POLL);
        }
        egui::Panel::top("top").show(ui, |ui| self.render_top(ui));
        egui::Panel::bottom("bottom").show(ui, |ui| self.render_bottom(ui));
        // 中央パネルは最後に追加する（ルール au2-rs-plugin「egui レイアウト」）
        egui::CentralPanel::default().show(ui, |ui| self.render_scope(ui));
    }
}

#[cfg(test)]
mod tests {
    //! しきい値の欄を egui だけで動かし、打っている途中の値が使われないこと・Esc で戻ることを確かめる

    use super::*;

    /// テストの 1 フレームの出力を捨てる。テクスチャの差分を片付けずに捨てると、デバッグビルドで epaint の debug_assert
    /// （Dropped TexturesDelta with N unapplied deltas）が落ちる（ルール au2-rs-plugin）
    fn discard_frame(mut out: egui::FullOutput) {
        out.textures_delta.clear();
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    /// 1 フレーム描く。`focus` なら描く前に欄へフォーカスを移す（クリックしたときと同じく全選択で編集に入る）
    fn frame(ctx: &egui::Context, value: &mut f32, id: Option<egui::Id>, focus: bool, events: Vec<egui::Event>) -> egui::Id {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))),
            events,
            ..Default::default()
        };
        let mut out = egui::Id::NULL;
        discard_frame(ctx.run_ui(input, |ui| {
            if let (true, Some(id)) = (focus, id) {
                ui.memory_mut(|m| m.request_focus(id));
            }
            out = percent_value(ui, value).id;
        }));
        out
    }

    /// 99.0 の欄に "90" を打ったところまで進める
    fn typed(ctx: &egui::Context, v: &mut f32) -> egui::Id {
        let id = frame(ctx, v, None, false, vec![]);
        frame(ctx, v, Some(id), true, vec![]);
        frame(ctx, v, Some(id), false, vec![egui::Event::Text("90".into())]);
        id
    }

    #[test]
    fn typed_value_is_used_only_after_enter() {
        let ctx = egui::Context::default();
        let mut v = 99.0;
        let id = typed(&ctx, &mut v);
        assert_eq!(v, 99.0, "打っている途中の値を使っている");
        frame(&ctx, &mut v, Some(id), false, vec![key(egui::Key::Enter)]);
        frame(&ctx, &mut v, Some(id), false, vec![]);
        assert_eq!(v, 90.0);
    }

    #[test]
    fn value_is_committed_when_focus_moves_away() {
        let ctx = egui::Context::default();
        let mut v = 99.0;
        let id = typed(&ctx, &mut v);
        ctx.memory_mut(|m| m.surrender_focus(id));
        for _ in 0..3 {
            frame(&ctx, &mut v, Some(id), false, vec![]);
        }
        assert_eq!(v, 90.0);
    }

    #[test]
    fn escape_restores_value_before_typing() {
        let ctx = egui::Context::default();
        let mut v = 99.0;
        let id = typed(&ctx, &mut v);
        frame(&ctx, &mut v, Some(id), false, vec![key(egui::Key::Escape)]);
        // フォーカスを失った後の 2 フレームも「失った」と扱われるので、数フレーム先まで戻らないこと
        for _ in 0..3 {
            frame(&ctx, &mut v, Some(id), false, vec![]);
        }
        assert_eq!(v, 99.0);
    }
}
