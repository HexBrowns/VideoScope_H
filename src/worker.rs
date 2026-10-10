//! 集計のスレッド（1 本）と、本体のコールバック・UI との受け渡し。
//!
//! - 本体のコールバック（イベント通知のスレッド）は、画素を写して `put` するだけ（重いことをしない）
//! - 集計のスレッドは最新の 1 枚だけを持つ。追いつかなければ古いものを捨てる
//! - 設定（種類・強度・間引き）が変わったら、最後に受け取った画素で描き直す
//! - プラグインの `Drop` で `stop` を呼び、終わりを待つ（ルール au2-rs-plugin「裏のスレッド」）

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::scope::{self, Image, Options};

/// 1 枚の出どころ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 止まっているとき、`rendering_scene_video` で本体に描かせた
    Render,
    /// 再生中、FullSizePreview_H の受け口から共有メモリで受け取った
    Receiver,
}

/// 本体から受け取った 1 枚（コールバックの中で写したもの）
pub struct Job {
    pub raw: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub pitch: usize,
    pub background: (u8, u8, u8),
    pub frame: u32,
    /// 依頼してからコールバックが来るまで（本体の描画の時間）。受け口からなら読み出しの時間
    pub render_ms: f32,
    pub source: Source,
}

/// 描いた結果
pub struct Output {
    pub image: Image,
    pub frame: u32,
    pub render_ms: f32,
    pub source: Source,
    pub calc_ms: f32,
    /// 測った画像の大きさ（間引いた後）
    pub sampled: (usize, usize),
}

#[derive(Default)]
struct State {
    job: Option<Job>,
    /// 最後に受け取った画素（設定が変わったときに描き直す）
    last: Option<Job>,
    redo: bool,
    output: Option<Output>,
    opts: Options,
    step: usize,
}

pub struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    stop: AtomicBool,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        let st = State { step: 2, ..Default::default() };
        Arc::new(Shared { state: Mutex::new(st), cv: Condvar::new(), stop: AtomicBool::new(false) })
    }

    /// 本体のコールバックから: 新しい 1 枚を置く（前の未処理の 1 枚は捨てる）
    pub fn put(&self, job: Job) {
        if let Ok(mut st) = self.state.lock() {
            st.job = Some(job);
        }
        self.cv.notify_one();
    }

    /// UI から: 設定を変える。変わっていれば最後の画素で描き直す
    pub fn set_options(&self, opts: Options, step: usize) {
        if let Ok(mut st) = self.state.lock() {
            if st.opts != opts || st.step != step {
                st.opts = opts;
                st.step = step.max(1);
                st.redo = true;
                self.cv.notify_one();
            }
        }
    }

    /// UI から: 新しい結果があれば受け取る
    pub fn take_output(&self) -> Option<Output> {
        self.state.lock().ok()?.output.take()
    }
}

pub struct Worker {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(shared: Arc<Shared>, on_done: impl Fn() + Send + 'static) -> Self {
        let s = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("VideoScope_H 集計".into())
            .spawn(move || run(s, on_done))
            .ok();
        if handle.is_none() {
            tracing::error!("[VideoScope_H] 集計のスレッドを起こせませんでした");
        }
        Worker { shared, handle }
    }

    /// 止めて終わりを待つ（上限 3 秒。超えたら警告して手を放す）
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.cv.notify_all();
        if let Some(h) = self.handle.take() {
            let t = Instant::now();
            while !h.is_finished() {
                if t.elapsed() > Duration::from_secs(3) {
                    tracing::warn!("[VideoScope_H] 集計のスレッドが 3 秒で止まりませんでした");
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = h.join();
        }
    }
}

fn run(shared: Arc<Shared>, on_done: impl Fn()) {
    loop {
        // 仕事を待つ
        let (job_is_new, job, opts, step) = {
            let Ok(mut st) = shared.state.lock() else { return };
            loop {
                if shared.stop.load(Ordering::Acquire) {
                    return;
                }
                if st.job.is_some() || (st.redo && st.last.is_some()) {
                    break;
                }
                st = match shared.cv.wait_timeout(st, Duration::from_millis(500)) {
                    Ok((g, _)) => g,
                    Err(_) => return,
                };
            }
            st.redo = false;
            let opts = st.opts;
            let step = st.step;
            match st.job.take() {
                Some(j) => (true, j, opts, step),
                None => {
                    let l = st.last.take().expect("last があることを確かめた");
                    (false, l, opts, step)
                }
            }
        };

        let t = Instant::now();
        let frame = scope::composite(&job.raw, job.width, job.height, job.pitch, job.background, step);
        let out = frame.map(|f| {
            let image = scope::render(&f, &opts);
            Output {
                image,
                frame: job.frame,
                render_ms: if job_is_new { job.render_ms } else { -1.0 },
                source: job.source,
                calc_ms: t.elapsed().as_secs_f32() * 1000.0,
                sampled: (f.width, f.height),
            }
        });
        if let Ok(mut st) = shared.state.lock() {
            if let Some(o) = out {
                st.output = Some(o);
            }
            // 描き直し用に残す（新しい 1 枚が来ていればそちらが優先される）
            st.last = Some(job);
        }
        on_done();
    }
}
