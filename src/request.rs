//! 止まっているときの描画の依頼（`rendering_scene_video`）。
//!
//! - 本体のイベント（フレームの移動・オブジェクトの更新・シーンの変更）では印を立てるだけ（`mark_dirty`）
//! - UI のスレッドの `poll` で、印が立っていて前の依頼から 100ms 以上たっていれば 1 回だけ頼む。
//!   **依頼中は重ねない**（最後の 1 回だけ追いかける）。コールバックが 2 秒来なければ依頼中を解く
//! - 依頼は描画のタスクを積むだけで、結果はイベント通知のスレッドからコールバックで来る（aviutl2 0.48 の
//!   `EditHandle::rendering_scene_video` の説明）。コールバックでは画素を写して集計のスレッドへ渡すだけ
//! - **再生中・出力中は頼まない**（本体の描画と同じ順番待ちに並び、キャッシュも効かない。FullSizePreview_H v0.2.0 で
//!   再生中に頼んで 3.5 fps まで落ちた）
//! - **再生中は、FullSizePreview_H の受け口（シーンの一番下に置くカスタムオブジェクト）が読んだフレームバッファを
//!   共有メモリで受け取る**（`share.rs`。受け口を共有するのはユーザーの決定、2026-10-10）。欲しい期限を 600ms 先に書き、
//!   受け口は 100ms ごとに 1 枚書く。期限が切れれば受け口は読まない

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aviutl2::generic::EditState;
use parking_lot::Mutex;

use crate::share;
use crate::worker::{Job, Shared, Source};
use crate::EDIT_HANDLE;

/// 描き直しが要る（イベントで立て、依頼したら下ろす）
pub static DIRTY: AtomicBool = AtomicBool::new(true);
/// 依頼中（コールバックで下ろす）
static INFLIGHT: AtomicBool = AtomicBool::new(false);
static LAST: Mutex<Option<Instant>> = Mutex::new(None);

const MIN_INTERVAL: Duration = Duration::from_millis(100);
const GIVE_UP: Duration = Duration::from_secs(2);

pub fn mark_dirty() {
    DIRTY.store(true, Ordering::Release);
}

/// UI に出す状態
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Waiting,
    Requested,
    /// 再生中で、受け口から届いている。`empty` は届いた画面が全部透明（受け口がカメラ制御の対象に入っている）
    PlayingLive { empty: bool },
    /// 再生中で、まだ届いていない（始まって 1 秒まで）
    PlayingWaiting,
    /// 再生中で、1 秒たっても届かない（受け口が無いか、受け渡しのできない古い版）
    PlayingNoReceiver,
    /// 共有メモリを開けなかった
    PlayingNoShare,
    Saving,
    Failed(String),
}

/// 再生中に受け口へ欲しがる間隔と期限
const PLAY_INTERVAL_MS: u32 = 100;
const PLAY_HOLD_MS: u64 = 600;
const NO_RECEIVER_AFTER: Duration = Duration::from_secs(1);

struct Play {
    /// 再生を見つけた時刻（None なら再生中でない）
    started: Option<Instant>,
    /// 同じ時刻の共有メモリの時計。これより前に書かれた 1 枚（前の再生の残り）は使わない
    started_ms: u64,
    last_seq: u32,
    last_frame_at: Option<Instant>,
    empty: bool,
    /// 再生ごとの集計（終わりにログへ 1 行出す）
    frames: u32,
    readback_sum_ms: f32,
    readback_max_ms: f32,
    empty_frames: u32,
}

static PLAY: Mutex<Play> = Mutex::new(Play {
    started: None,
    started_ms: 0,
    last_seq: 0,
    last_frame_at: None,
    empty: false,
    frames: 0,
    readback_sum_ms: 0.0,
    readback_max_ms: 0.0,
    empty_frames: 0,
});

/// 透明でない画素が 1 つも無いか（64 画素に 1 つを見る）
fn all_transparent(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).step_by(64).all(|p| p[3] == 0)
}

/// 再生中: 欲しい期限を延ばし、新しい 1 枚があれば集計へ渡す
fn poll_playing(shared: &Arc<Shared>) -> Status {
    let Some(region) = share::region() else { return Status::PlayingNoShare };
    let now = share::now_ms();
    region.request(now, PLAY_HOLD_MS, PLAY_INTERVAL_MS);
    let mut p = PLAY.lock();
    let started = *p.started.get_or_insert_with(Instant::now);
    if p.started_ms == 0 {
        p.started_ms = now;
    }
    let mut buf = Vec::new();
    if let Some(info) = region.read(p.last_seq, &mut buf) {
        p.last_seq = info.seq;
        if info.published_ms >= p.started_ms {
            p.last_frame_at = Some(Instant::now());
            p.empty = all_transparent(&buf);
            let rb = info.readback_us as f32 / 1000.0;
            p.frames += 1;
            p.readback_sum_ms += rb;
            p.readback_max_ms = p.readback_max_ms.max(rb);
            p.empty_frames += p.empty as u32;
            let background = EDIT_HANDLE.get_edit_info().background.unwrap_or((0, 0, 0));
            shared.put(Job {
                raw: buf,
                width: info.width as usize,
                height: info.height as usize,
                pitch: info.width as usize * 4,
                background,
                frame: info.frame,
                render_ms: info.readback_us as f32 / 1000.0,
                source: Source::Receiver,
            });
        }
    }
    match p.last_frame_at {
        Some(t) if t.elapsed() < NO_RECEIVER_AFTER => Status::PlayingLive { empty: p.empty },
        _ if started.elapsed() < NO_RECEIVER_AFTER => Status::PlayingWaiting,
        _ => Status::PlayingNoReceiver,
    }
}

/// 再生が終わった: 欲しがるのをやめ、今のフレームを描き直す
fn end_playing() {
    let mut p = PLAY.lock();
    if let Some(started) = p.started.take() {
        // 速さはログで測る（ルール au2-host-exceptions）。受け口の読み出しは本体の描画スレッドの中の時間
        let secs = started.elapsed().as_secs_f32();
        let n = p.frames;
        tracing::info!(
            "[VideoScope_H] 再生 {:.1} 秒 / 受け口から {} 枚（{:.1} 枚/秒）/ 読み出し 平均 {:.1}ms 最大 {:.1}ms / 空の画面 {} 枚",
            secs,
            n,
            if secs > 0.0 { n as f32 / secs } else { 0.0 },
            if n > 0 { p.readback_sum_ms / n as f32 } else { 0.0 },
            p.readback_max_ms,
            p.empty_frames
        );
        p.frames = 0;
        p.readback_sum_ms = 0.0;
        p.readback_max_ms = 0.0;
        p.empty_frames = 0;
        p.started_ms = 0;
        p.last_frame_at = None;
        p.empty = false;
        if let Some(region) = share::region() {
            region.cancel();
        }
        mark_dirty();
    }
}

/// UI のスレッドから毎回呼ぶ。頼んだら Requested
pub fn poll(shared: &Arc<Shared>) -> Status {
    if !EDIT_HANDLE.is_ready() {
        return Status::Waiting;
    }
    match EDIT_HANDLE.get_edit_state() {
        Ok(EditState::Preview) => return poll_playing(shared),
        Ok(EditState::Save) => {
            end_playing();
            return Status::Saving;
        }
        _ => end_playing(),
    }
    let mut last = LAST.lock();
    if INFLIGHT.load(Ordering::Acquire) {
        if last.is_some_and(|t| t.elapsed() > GIVE_UP) {
            tracing::warn!("[VideoScope_H] 描画の結果が 2 秒来なかったので、待つのをやめました");
            INFLIGHT.store(false, Ordering::Release);
        } else {
            return Status::Requested;
        }
    }
    if !DIRTY.load(Ordering::Acquire) || last.is_some_and(|t| t.elapsed() < MIN_INTERVAL) {
        return Status::Waiting;
    }
    let info = EDIT_HANDLE.get_edit_info();
    let background = info.background.unwrap_or((0, 0, 0));
    let frame = info.frame as u32;
    let started = Instant::now();
    let sh = Arc::clone(shared);
    DIRTY.store(false, Ordering::Release);
    INFLIGHT.store(true, Ordering::Release);
    *last = Some(started);
    let res = EDIT_HANDLE.rendering_scene_video(frame, move |v| {
        let width = v.width as usize;
        let height = v.height as usize;
        let pitch = v.pitch as usize;
        if !v.buffer.is_empty() && width > 0 && height > 0 {
            sh.put(Job {
                raw: v.buffer.to_vec(),
                width,
                height,
                pitch,
                background,
                frame: v.frame,
                render_ms: started.elapsed().as_secs_f32() * 1000.0,
                source: Source::Render,
            });
        }
        INFLIGHT.store(false, Ordering::Release);
        if let Some(ctx) = crate::EGUI_CTX.get() {
            ctx.request_repaint();
        }
    });
    match res {
        Ok(()) => Status::Requested,
        Err(e) => {
            INFLIGHT.store(false, Ordering::Release);
            // 頼めなかった分は次の回に持ち越す（出力中などは失敗する。plugin2.h の rendering_scene_video）
            DIRTY.store(true, Ordering::Release);
            Status::Failed(format!("描画を頼めませんでした: {e}"))
        }
    }
}
