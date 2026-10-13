//! VideoScope_H（ビデオスコープ）
//!
//! 今のフレームの輝度の波形・RGB パレード・RGB の重ね・ベクトルスコープ・ヒストグラムをウィンドウに描く。
//! 止まっているときは、カーソルのフレームを `rendering_scene_video` で本体に描かせて測る（`request.rs`）。
//! 再生中は、FullSizePreview_H の受け口（シーンの一番下に置くカスタムオブジェクト）が読んだフレームバッファを
//! 共有メモリで受け取る（`share.rs`。仕様の 4.2。受け口を FullSizePreview_H と共有するのはユーザーの決定）。
//! 仕様は `AI/specifications/20261009_VideoScope_H_spec.md`（YMM4 の MuchaMonitor・スコープモニターに当たる）。
//!
//! 自分で起こすスレッドは集計の 1 本（`worker.rs`）。プラグインの `Drop` で止めて終わりを待つ（ルール au2-rs-plugin）。

mod host_fonts;
mod gui;
mod request;
pub mod scope;
pub mod share;
mod worker;

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, OnceLock};

use aviutl2::generic::{EventType, GlobalEditHandle};
use aviutl2::raw_window_handle::HasWindowHandle;
use aviutl2::AnyResult;
use aviutl2_eframe::egui;
use parking_lot::Mutex;

pub const WINDOW_NAME: &str = "ビデオスコープ";

pub(crate) static EDIT_HANDLE: GlobalEditHandle = GlobalEditHandle::new();

/// ウィンドウの HWND（見えているかを調べるため）
static WINDOW_HWND: AtomicIsize = AtomicIsize::new(0);

/// UI のスレッドを起こすための egui の Context（ウィンドウができたら入る）
pub(crate) static EGUI_CTX: OnceLock<egui::Context> = OnceLock::new();

fn init_logging() {
    use aviutl2::tracing::Level;
    let level = if cfg!(debug_assertions) { Level::DEBUG } else { Level::INFO };
    let _ = aviutl2::tracing_subscriber::fmt()
        .with_max_level(level)
        .event_format(aviutl2::logger::AviUtl2Formatter)
        .with_writer(aviutl2::logger::AviUtl2LogWriter)
        .try_init();
}

/// 本体のイベント: 描き直しの印を立てて UI を起こす（ここから本体を呼ばない）
fn on_host_event() {
    request::mark_dirty();
    if let Some(ctx) = EGUI_CTX.get() {
        ctx.request_repaint();
    }
}

/// ウィンドウが見えているか（本体のパネルに出ていなければ偽）。ハンドルが取れていなければ見えているとみなす
pub(crate) fn window_visible() -> bool {
    let hwnd = WINDOW_HWND.load(Ordering::Acquire);
    if hwnd == 0 {
        return true;
    }
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(windows::Win32::Foundation::HWND(hwnd as _)).as_bool() }
}

/// 初期化にかかった時間をログに 1 行出す（強化デバッグ。仕様 `AI/specifications/20261012_CurveEditor_H_キーフレーム速度_spec.md` の 7.0）。
/// `register` の始めに作り、`register` を抜けるとき（途中の `return` も含む）に new と register の時間を出す
struct InitTimeLog {
    new_time: std::time::Duration,
    register_start: std::time::Instant,
}

impl InitTimeLog {
    fn start(new_time: std::time::Duration) -> Self {
        Self { new_time, register_start: std::time::Instant::now() }
    }
}

impl Drop for InitTimeLog {
    fn drop(&mut self) {
        let register = self.register_start.elapsed();
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        aviutl2::tracing::info!(
            "VideoScope_H: 初期化 {:.1} ms（new {:.1} ms・register {:.1} ms）",
            ms(self.new_time + register),
            ms(self.new_time),
            ms(register)
        );
    }
}

#[aviutl2::plugin(GenericPlugin)]
pub struct VideoScopePlugin {
    window: Mutex<Option<aviutl2_eframe::EframeWindow>>,
    shared: Arc<worker::Shared>,
    worker: Mutex<Option<worker::Worker>>,
    /// `new` にかかった時間（初期化の時間をログに出すため。`InitTimeLog`）
    init_new_time: std::time::Duration,
}

impl aviutl2::generic::GenericPlugin for VideoScopePlugin {
    fn new(_info: aviutl2::AviUtl2Info) -> AnyResult<Self> {
        let init_t0 = std::time::Instant::now();
        init_logging();
        tracing::info!("VideoScope_H v{} 初期化", env!("CARGO_PKG_VERSION"));
        let shared = worker::Shared::new();
        let w = worker::Worker::start(Arc::clone(&shared), || {
            if let Some(ctx) = EGUI_CTX.get() {
                ctx.request_repaint();
            }
        });
        Ok(Self { window: Mutex::new(None), shared, worker: Mutex::new(Some(w)), init_new_time: init_t0.elapsed() })
    }

    fn plugin_info(&self) -> aviutl2::generic::GenericPluginTable {
        aviutl2::generic::GenericPluginTable {
            name: "VideoScope_H".to_string(),
            information: format!("VideoScope_H v{} - ビデオスコープ / by HexBrowns", env!("CARGO_PKG_VERSION")),
        }
    }

    fn register(&mut self, registry: &mut aviutl2::generic::HostAppHandle) {
        // 抜けるときに初期化の時間をログに出す（`InitTimeLog`）
        let _init_time = InitTimeLog::start(self.init_new_time);
        EDIT_HANDLE.init(registry.create_edit_handle());
        let shared = Arc::clone(&self.shared);
        let window = match aviutl2_eframe::EframeWindow::new(WINDOW_NAME, move |cc, handle| {
            Ok(Box::new(gui::ScopeApp::new(cc, handle, Arc::clone(&shared))))
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!("ビデオスコープのウィンドウを作れませんでした: {e:#}");
                return;
            }
        };
        match window.handle() {
            Ok(handle) => {
                if let Err(e) = registry.register_window_client(WINDOW_NAME, &handle) {
                    tracing::error!("register_window_client 失敗: {e}");
                }
                if let Ok(raw) = handle.window_handle() {
                    if let aviutl2::raw_window_handle::RawWindowHandle::Win32(h) = raw.as_raw() {
                        WINDOW_HWND.store(h.hwnd.get(), Ordering::Release);
                    }
                }
            }
            Err(e) => tracing::error!("ビデオスコープのウィンドウのハンドルを取れませんでした: {e:#}"),
        }
        *self.window.lock() = Some(window);
        // ウィンドウは表示メニューから開く（メニューから ShowWindow しても開かない。2026-10-10 にユーザーの指摘で外した）

        // 描き直しの合図（フレームの移動・オブジェクトの更新・シーンの変更）
        for ev in [EventType::ChangeEditFrame, EventType::UpdateObject, EventType::ChangeEditScene] {
            registry.register_event_listener(ev, on_host_event);
        }
    }
}

impl Drop for VideoScopePlugin {
    fn drop(&mut self) {
        // 集計のスレッドを止めて終わりを待つ（DLL が外れた後に動くと終了時に落ちる。ルール au2-rs-plugin）
        if let Some(mut w) = self.worker.lock().take() {
            w.stop();
        }
    }
}

// 本体が呼ぶ関数（GetCommonPluginTable など）を書き出す（ルール au2-rs-plugin「ビルド」）
aviutl2::register_generic_plugin!(VideoScopePlugin);
