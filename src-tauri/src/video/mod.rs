// src-tauri/src/video/mod.rs
// 视频处理模块 - 更新版本

use crate::audio::SilenceSegment;
use serde::{Deserialize, Serialize};
use serde_json;
use std::path::Path;
use tokio::process::Command as TokioCommand;
use std::fs;
use std::io::Write;
use tauri::Emitter;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// ... [skipping middle part for brevity in internal thought but will use full lines in tool call]


// 视频信息
#[derive(Debug, Serialize, Deserialize)]
pub struct VideoInfo {
    pub path: String,
    pub filename: String,
    pub size_bytes: u64,
    pub duration: f64,
    pub format: Option<String>,
    pub codec_video: Option<String>,
    pub codec_audio: Option<String>,
    pub resolution: Option<(u32, u32)>,
    pub framerate: Option<f64>,
    pub bitrate: Option<u64>,
    pub has_video: bool,
    pub has_audio: bool,
}

// 处理结果
#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessResult {
    pub input_path: String,
    pub output_path: String,
    pub original_duration: f64,
    pub processed_duration: f64,
    pub silence_segments: usize,
    pub total_silence_removed: f64,
    pub compression_ratio: f64,
    pub processing_time: f64,
    pub success: bool,
    pub error_message: Option<String>,
}

// 进度回调
pub type ProgressCallback = Box<dyn Fn(f64) + Send>;

// 获取视频信息
pub async fn get_video_info(ffprobe_path: &str, video_path: &str) -> Result<VideoInfo, Box<dyn std::error::Error>> {
    
    #[derive(Deserialize)]
    struct FfprobeOutput {
    streams: Option<Vec<FfprobeStream>>,
    format: Option<FfprobeFormat>,
   }

  #[derive(Deserialize)]
   struct FfprobeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    duration: Option<String>,
   }

    #[derive(Deserialize)]
    struct FfprobeFormat {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
   }
    
    let mut cmd = TokioCommand::new(ffprobe_path);
    cmd.args(&["-v", "quiet", "-print_format", "json", "-show_format", "-show_streams", video_path]);
    
    let output = cmd.output().await?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        log::error!("FFprobe failed for {}: {}", video_path, err);
        return Err(format!("FFprobe 执行失败: {}", err).into());
    }
    
    // 一键反序列化成强类型对象，告警和拼写错误在编译期就被杜绝了
    let probe_data: FfprobeOutput = serde_json::from_slice(&output.stdout)?;
    
    let mut has_video = false;
    let mut has_audio = false;
    let mut codec_video = None;
    let mut codec_audio = None;
    let mut resolution = None;
    let mut framerate = None;
    let mut backup_duration = 0.0;

    // 遍历强类型数组，摆脱 .as_array() 和字符串字面量键
    if let Some(streams) = probe_data.streams {
        for stream in streams {
            match stream.codec_type.as_deref() {
                Some("video") => {
                    has_video = true;
                    codec_video = stream.codec_name;
                    
                    if let (Some(w), Some(h)) = (stream.width, stream.height) {
                        resolution = Some((w, h));
                    }
                    
                    if let Some(cfr) = stream.avg_frame_rate.as_deref() {
                        framerate = parse_framerate(cfr);
                    }
                    
                    if backup_duration == 0.0 {
                        backup_duration = stream.duration.and_then(|d| d.parse::<f64>().ok()).unwrap_or(0.0);
                    }
                }
                Some("audio") => {
                    has_audio = true;
                    codec_audio = stream.codec_name;
                }
                _ => {}
            }
        }
    }

    // 基础文件元数据提取
    let size_bytes = fs::metadata(video_path)?.len();
    let filename = Path::new(video_path)
        .file_name()
        .and_then(|n: &std::ffi::OsStr| n.to_str()) 
        .unwrap_or("unknown")
        .to_string();
    
    // 从强类型 Format 抽取
    let (format, duration, bitrate) = match probe_data.format {
        Some(fmt) => {
            let d = fmt.duration.and_then(|d| d.parse::<f64>().ok()).unwrap_or(backup_duration);
            let b = fmt.bit_rate.and_then(|b| b.parse::<u64>().ok());
            (fmt.format_name, d, b)
        }
        None => (None, backup_duration, None),
    };
    
    Ok(VideoInfo {
        path: video_path.to_string(),
        filename,
        size_bytes,
        duration,
        format,
        codec_video,
        codec_audio,
        resolution,
        framerate,
        bitrate,
        has_video,
        has_audio,
    })
}

// === 3. 剥离出的纯计算辅助函数：帧率解析 ===
fn parse_framerate(fps_str: &str) -> Option<f64> {
    let (num_str, den_str) = fps_str.split_once('/')?;
    let n = num_str.parse::<f64>().ok()?;
    let d = den_str.parse::<f64>().ok()?;
    if d != 0.0 { Some(n / d) } else { None }
}

// 内部使用的片段结构
#[derive(Debug, Clone)]
struct SpeechSegment {
    start: f64,
    end: f64,
}

// ============================================================
// 重构：将 remove_silence_from_video 拆分为多个职责单一的子模块
// ============================================================

/// 说话片段计算器：从静音片段列表计算出需要保留的说话片段
struct SpeechSegmentCalculator;

impl SpeechSegmentCalculator {
    /// 从静音片段和视频总时长计算说话片段
    fn calculate(silences: &[SilenceSegment], original_duration: f64) -> Vec<SpeechSegment> {
        let mut speech_segments = Vec::new();
        let mut last_end = 0.0;
        
        // 增加一个小于 0.1s 的容差，避免各种浮点数精度或 ffprobe 误差导致的"幽灵尾巴"
        let timestamp_tolerance = 0.05;

        for silence in silences {
            // 如果当前静音开始时间远大于上一个结束时间，说明中间有一段说话
            if silence.start_time > last_end + timestamp_tolerance {
                speech_segments.push(SpeechSegment { start: last_end, end: silence.start_time });
            }
            last_end = silence.end_time;
        }

        // 处理最后一段说话（直到视频结束）
        // 特别注意：如果最后一段太短（比如小于 0.1s），通常是 ffprobe 时长的误差，应该直接忽略
        if last_end < original_duration - 0.1 {
            speech_segments.push(SpeechSegment { start: last_end, end: original_duration });
        }

        // 再次过滤：删除任何由于逻辑计算产生的极短片段（小于一个 GOB 或一帧的量级）
        speech_segments.retain(|s| (s.end - s.start) > 0.05);

        speech_segments
    }
}

/// 进度通知器：封装所有与 tauri::Window 相关的进度通知逻辑
struct ProgressNotifier {
    window: Option<tauri::Window>,
}

impl ProgressNotifier {
    fn new(window: Option<tauri::Window>) -> Self {
        Self { window }
    }

    fn emit(&self, percent: f64, message_key: &str, params: serde_json::Value, eta: f64) {
        if let Some(ref win) = self.window {
            let _ = win.emit("video-progress", serde_json::json!({
                "percent": percent,
                "message_key": message_key,
                "params": params,
                "eta": eta
            }));
        }
    }

    fn emit_probe_start(&self) {
        self.emit(0.5, "probe_start", serde_json::json!({}), 0.0);
    }

    fn emit_analysis(&self) {
        self.emit(1.0, "analysis", serde_json::json!({}), 0.0);
    }

    fn emit_init_parallel(&self, num_batches: usize) {
        self.emit(2.0, "init_parallel", serde_json::json!({"batches": num_batches}), 0.0);
    }

    fn emit_submit_batch(&self, batch_idx: usize, num_batches: usize) {
        self.emit(2.0, "submit_batch", serde_json::json!({"current": batch_idx + 1, "total": num_batches}), 0.0);
    }

    fn emit_batch_completed(&self, completed: usize, num_batches: usize, elapsed: f64) {
        let avg_time_per_batch = elapsed / completed as f64;
        let remaining_batches = num_batches - completed;
        let eta = avg_time_per_batch * remaining_batches as f64;
        let progress = 1.0 + (completed as f64 / num_batches as f64 * 90.0);

        self.emit(progress, "batch_completed", serde_json::json!({"completed": completed, "total": num_batches}), eta);
    }

    fn emit_merging(&self) {
        self.emit(95.0, "merging", serde_json::json!({}), 1.0);
    }

    fn emit_complete(&self) {
        self.emit(100.0, "complete", serde_json::json!({}), 0.0);
    }
}

/// 并行批次处理器：负责将说话片段分批并行转码
struct BatchProcessor<'a> {
    ffmpeg_path: &'a str,
    input_path: &'a str,
    temp_dir: &'a Path,
    speech_segments: &'a [SpeechSegment],
    has_video: bool,
    original_bitrate: Option<u64>,
    segments_per_batch: usize,
    max_concurrent_tasks: usize,
    notifier: &'a ProgressNotifier,
    cancel_signal: &'a Arc<AtomicBool>,
}

impl<'a> BatchProcessor<'a> {
    fn new(
        ffmpeg_path: &'a str,
        input_path: &'a str,
        temp_dir: &'a Path,
        speech_segments: &'a [SpeechSegment],
        has_video: bool,
        original_bitrate: Option<u64>,
        notifier: &'a ProgressNotifier,
        cancel_signal: &'a Arc<AtomicBool>,
    ) -> Self {
        Self {
            ffmpeg_path,
            input_path,
            temp_dir,
            speech_segments,
            has_video,
            original_bitrate,
            segments_per_batch: 10,
            max_concurrent_tasks: 4,
            notifier,
            cancel_signal,
        }
    }

    /// 计算批次数量
    fn num_batches(&self) -> usize {
        (self.speech_segments.len() + self.segments_per_batch - 1) / self.segments_per_batch
    }

    /// 执行所有批次的并行转码
    async fn process(&self) -> Result<usize, Box<dyn std::error::Error>> {
        let num_batches = self.num_batches();
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(self.max_concurrent_tasks));
        let mut tasks = tokio::task::JoinSet::new();
        let start_processing_time = std::time::Instant::now();

        let ffmpeg_path_str = self.ffmpeg_path.to_string();
        for batch_idx in 0..num_batches {
            let start_idx = batch_idx * self.segments_per_batch;
            let end_idx = (start_idx + self.segments_per_batch).min(self.speech_segments.len());
            
            let batch_segments = self.speech_segments[start_idx..end_idx].to_owned();
            let input = self.input_path.to_string();
            let batch_output = self.temp_dir.join(format!("part_{}.ts", batch_idx));
            let has_video = self.has_video;
            let sem = semaphore.clone();
            let original_bitrate = self.original_bitrate;
            let ffmpeg_cmd = ffmpeg_path_str.clone();

            // 计算该批次的快速寻址起点：取该批第一个片段的 start
            let seek_start = batch_segments[0].start;

            self.notifier.emit_submit_batch(batch_idx, num_batches);

            tasks.spawn(async move {
                let _permit = sem.acquire().await.map_err(|e| format!("Semaphore error: {}", e))?;
                process_batch_to_ts(
                    &ffmpeg_cmd,
                    &input, 
                    batch_output.to_str().unwrap(), 
                    &batch_segments, 
                    has_video, 
                    seek_start,
                    original_bitrate
                ).await
            });
        }

        // 等待所有并行任务完成，同时监听取消信号
        let completed = self.await_completion(&mut tasks, num_batches, start_processing_time).await?;
        
        Ok(completed)
    }

    /// 等待所有任务完成，支持取消信号和进度通知
    async fn await_completion(
        &self,
        tasks: &mut tokio::task::JoinSet<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
        num_batches: usize,
        start_processing_time: std::time::Instant,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let mut completed = 0;
        while completed < num_batches {
            // 利用 tokio::select! 增强响应速度，避免 join_next() 阻塞期间无法响应取消信号
            tokio::select! {
                res = tasks.join_next() => {
                    if let Some(join_res) = res {
                        // 第一个 ? 处理 JoinError
                        let batch_result = join_res.map_err(|e| format!("Parallel task panicked: {}", e))?;
                        // 第二个 处理 batch 内部的 FFmpeg 错误
                        batch_result.map_err(|e| format!("Batch processing error: {}", e))?;
                        
                        completed += 1;
                        
                        let elapsed = start_processing_time.elapsed().as_secs_f64();
                        self.notifier.emit_batch_completed(completed, num_batches, elapsed);
                    } else {
                        break;
                    }
                }
                // 每隔 100ms 检查一次取消信号，大幅降低延迟
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                    if self.cancel_signal.load(Ordering::SeqCst) {
                        tasks.abort_all();
                        return Err("EXPORT_CANCELLED".into());
                    }
                }
            }
        }
        Ok(completed)
    }
}

/// 片段合并器：使用 FFmpeg Concat Demuxer 合并临时 TS 片段
struct ConcatMerger;

impl ConcatMerger {
    /// 合并指定数量的临时 TS 片段到最终输出文件
    async fn merge(
        ffmpeg_path: &str,
        temp_dir: &Path,
        output_path: &str,
        num_batches: usize,
        has_video: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let concat_file_path = temp_dir.join("list.txt");
        let mut concat_file = fs::File::create(&concat_file_path)?;
        for i in 0..num_batches {
            // 确保按顺序写入
            writeln!(concat_file, "file 'part_{}.ts'", i)?;
        }
        concat_file.flush()?;

        let mut concat_cmd = TokioCommand::new(ffmpeg_path);
        concat_cmd.args(&[
            "-f", "concat",
            "-safe", "0",
            "-i", concat_file_path.to_str().unwrap(),
        ]);

        if has_video {
            // 视频：音视频流拷贝 + MP4 快速启动优化
            concat_cmd.args(&[
                "-c", "copy",
                "-movflags", "+faststart",
            ]);
        } else {
            // 纯音频：只拷贝音频流，不加 MP4 专属选项
            concat_cmd.args(&[
                "-c:a", "copy",
            ]);
        }

        concat_cmd.args(&["-y", output_path]);

        let status = concat_cmd.status().await?;
        
        if !status.success() {
            return Err("合并片段失败".into());
        }
        Ok(())
    }
}

/// 处理结果构建器：负责构建 ProcessResult
struct ProcessResultBuilder;

impl ProcessResultBuilder {
    fn build(
        input_path: &str,
        output_path: &str,
        original_duration: f64,
        processed_duration: f64,
        silence_segments: usize,
        total_silence_removed: f64,
        processing_time: f64,
        success: bool,
        error_message: Option<String>,
    ) -> ProcessResult {
        ProcessResult {
            input_path: input_path.to_string(),
            output_path: output_path.to_string(),
            original_duration,
            processed_duration,
            silence_segments,
            total_silence_removed,
            compression_ratio: if original_duration > 0.0 {
                (total_silence_removed / original_duration) * 100.0
            } else {
                0.0
            },
            processing_time,
            success,
            error_message,
        }
    }
}

/// 临时目录管理器：负责创建和清理临时目录
struct TempDirManager;

impl TempDirManager {
    /// 基于输出路径创建临时目录
    fn create(output_path: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let mut temp_dir = PathBuf::from(output_path);
        temp_dir.set_extension("temp_parts");
        if temp_dir.exists() {
            let _ = fs::remove_dir_all(&temp_dir);
        }
        fs::create_dir_all(&temp_dir)?;
        Ok(temp_dir)
    }

    /// 清理临时目录
    fn cleanup(temp_dir: &Path) {
        let _ = fs::remove_dir_all(temp_dir);
    }
}

/// 取消信号检查器：封装取消信号的检查逻辑
struct CancelChecker<'a> {
    cancel_signal: &'a Arc<AtomicBool>,
    temp_dir: &'a Path,
}

impl<'a> CancelChecker<'a> {
    fn new(cancel_signal: &'a Arc<AtomicBool>, temp_dir: &'a Path) -> Self {
        Self { cancel_signal, temp_dir }
    }

    /// 检查是否已取消，如果是则清理并返回错误
    fn check(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.cancel_signal.load(Ordering::SeqCst) {
            TempDirManager::cleanup(self.temp_dir);
            println!("🛑 任务被用户取消，正在清理临时文件...");
            return Err("EXPORT_CANCELLED".into());
        }
        Ok(())
    }
}

// 从视频移除静音 (加速并行版) - 重构后版本
pub async fn remove_silence_from_video(
    ffmpeg_path: &str,
    ffprobe_path: &str,
    input_path: &str,
    output_path: &str,
    silences: &[SilenceSegment],
    window: Option<tauri::Window>,
    cancel_signal: Arc<AtomicBool>,
) -> Result<ProcessResult, Box<dyn std::error::Error>> {
    let start_time = std::time::Instant::now();
    let notifier = ProgressNotifier::new(window);

    // 阶段 1: 获取视频信息
    notifier.emit_probe_start();
    let video_info = get_video_info(ffprobe_path, input_path).await?;
    let original_duration = video_info.duration;

    // 纯音频素材：输出强制使用 .m4a 容器（管线已将音频重编码为 AAC，原容器可能不兼容）
    let output_path = if !video_info.has_video {
        let mut p = PathBuf::from(output_path);
        p.set_extension("m4a");
        println!("🔊 检测到纯音频素材，输出格式调整为: {}", p.display());
        p.to_string_lossy().to_string()
    } else {
        output_path.to_string()
    };

    // 阶段 2: 分析片段
    notifier.emit_analysis();

    if silences.is_empty() {
        fs::copy(input_path, &output_path)?;
        return Ok(ProcessResultBuilder::build(
            input_path, &output_path,
            original_duration, original_duration,
            0, 0.0,
            start_time.elapsed().as_secs_f64(),
            true, None,
        ));
    }

    // 阶段 3: 计算说话片段
    let speech_segments = SpeechSegmentCalculator::calculate(silences, original_duration);

    if speech_segments.is_empty() {
        return Err("剪辑完成后没有剩余有效片段".into());
    }

    let total_silence_removed: f64 = silences.iter().map(|s| s.duration).sum();
    let processed_duration = original_duration - total_silence_removed;

    // 阶段 4: 创建临时目录
    let temp_dir = TempDirManager::create(&output_path)?;
    let cancel_checker = CancelChecker::new(&cancel_signal, &temp_dir);

    println!("🚀 工业级并行化: {} 片段 -> {} 批次 (每批 10)", 
        speech_segments.len(), (speech_segments.len() + 9) / 10);
    
    notifier.emit_init_parallel((speech_segments.len() + 9) / 10);

    // 阶段 5: 并行转码
    let batch_processor = BatchProcessor::new(
        ffmpeg_path,
        input_path,
        &temp_dir,
        &speech_segments,
        video_info.has_video,
        video_info.bitrate,
        &notifier,
        &cancel_signal,
    );

    let completed = batch_processor.process().await?;

    // 阶段 6: 检查取消信号
    cancel_checker.check()?;

    // 阶段 7: 合并片段
    println!("并行任务全部完成，正在合并 {} 个片段...", completed);
    notifier.emit_merging();
    
    ConcatMerger::merge(ffmpeg_path, &temp_dir, &output_path, completed, video_info.has_video).await?;

    // 阶段 8: 清理临时文件
    TempDirManager::cleanup(&temp_dir);

    // 阶段 9: 构建结果
    let processing_time = start_time.elapsed().as_secs_f64();
    println!("✅ 并行处理成功！耗时: {:.2}s", processing_time);
    notifier.emit_complete();

    Ok(ProcessResultBuilder::build(
        input_path, &output_path,
        original_duration, processed_duration,
        silences.len(), total_silence_removed,
        processing_time,
        true, None,
    ))
}

// 内部函数：处理一个批次的片段到一个 TS 文件
async fn process_batch_to_ts(
    ffmpeg_path: &str,
    input: &str,
    output: &str,
    segments: &[SpeechSegment],
    has_video: bool,
    seek_start: f64,
    original_bitrate: Option<u64>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut filter = String::new();
    let mut v_concat = String::new();
    let mut a_concat = String::new();

    for (i, seg) in segments.iter().enumerate() {
        // 关键点：时间必须减去 seek_start 的偏移量
        let s = (seg.start - seek_start).max(0.0);
        let duration = (seg.end - seg.start).max(0.0);

        if has_video {
            filter.push_str(&format!("[0:v]trim=start={:.3}:duration={:.3},setpts=PTS-STARTPTS[v{}];", s, duration, i));
            v_concat.push_str(&format!("[v{}]", i));
        }
        filter.push_str(&format!("[0:a]atrim=start={:.3}:duration={:.3},asetpts=PTS-STARTPTS[a{}];", s, duration, i));
        a_concat.push_str(&format!("[a{}]", i));
    }

    if has_video {
        filter.push_str(&format!("{}concat=n={}:v=1:a=0[fv];", v_concat, segments.len()));
    }
    filter.push_str(&format!("{}concat=n={}:v=0:a=1[fa]", a_concat, segments.len()));

    let mut cmd = TokioCommand::new(ffmpeg_path);
    
    // 关键优化：在前置位放置 -ss，利用 FFmpeg 的快速跳转能力 (Fast Input Seeking)
    cmd.args(&["-nostdin", "-ss", &seek_start.to_string(), "-i", input]);
    cmd.args(&["-filter_complex", &filter]);
    
    if has_video {
        cmd.args(&["-map", "[fv]"]);

        // 行业标准：比特率控制逻辑
        // 如果能获取到原始比特率，则作为目标比特率，否则使用 5000k 兜底
        let v_bitrate = match original_bitrate {
            Some(b) if b > 0 => {
                // 减去音频估算 (128kbps)，确保总比特率不超标
                let calc = b.saturating_sub(128_000);
                // 设定上下限：最低 1M 保证感官，最高 15M 防止异常大文件
                let kbps = (calc / 1000).clamp(1000, 15000);
                format!("{}k", kbps)
            },
            _ => "5000k".to_string(),
        };

        if cfg!(target_os = "macos") {
            // macOS 使用硬件加速，并严格遵循原视频比特率
            cmd.args(&[
                "-c:v", "h264_videotoolbox", 
                "-b:v", &v_bitrate,
                "-profile:v", "high",
                "-realtime", "true" 
            ]); 
        } else {
            // 其他平台使用 libx264，采用 CRF 保证质量 + maxrate 限制体积膨胀
            cmd.args(&[
                "-c:v", "libx264", 
                "-crf", "23",
                "-maxrate", &v_bitrate,
                "-bufsize", &format!("{}k", v_bitrate.trim_end_matches('k').parse::<u64>().unwrap_or(5000) * 2),
                "-preset", "superfast"
            ]);
        }
    }

    cmd.args(&["-map", "[fa]", "-c:a", "aac", "-b:a", "128k", "-f", "mpegts", "-y", output]);

    let output_res = cmd.output().await?;
    if !output_res.status.success() {
        return Err(format!("FFmpeg Batch Error").into());
    }
    Ok(())
}

// 构建过滤器 (此函数在旧版中使用，现已重构)
fn _build_filter_complex(silences: &[SilenceSegment], total_duration: f64, has_video: bool) -> String {
    let mut filter_parts = Vec::new();
    let mut concat_inputs = Vec::new();
    
    let mut last_end = 0.0;
    let mut segment_index = 0;
    
    for silence in silences.iter() {
        // 保留非静音片段（在静音之前）
        if silence.start_time > last_end {
            if has_video {
                // 视频片段
                filter_parts.push(format!(
                    "[0:v]trim=start={}:end={},setpts=PTS-STARTPTS[v{}]",
                    last_end, silence.start_time, segment_index
                ));
            }
            // 音频片段
            filter_parts.push(format!(
                "[0:a]atrim=start={}:end={},asetpts=PTS-STARTPTS[a{}]",
                last_end, silence.start_time, segment_index
            ));
            
            if has_video {
                concat_inputs.push(format!("[v{}][a{}]", segment_index, segment_index));
            } else {
                concat_inputs.push(format!("[a{}]", segment_index));
            }
            segment_index += 1;
        }
        
        last_end = silence.end_time;
    }
    
    // 添加最后的片段（静音之后到视频结束）
    if last_end < total_duration {
        if has_video {
            filter_parts.push(format!(
                "[0:v]trim=start={}:end={},setpts=PTS-STARTPTS[v{}]",
                last_end, total_duration, segment_index
            ));
        }
        filter_parts.push(format!(
            "[0:a]atrim=start={}:end={},asetpts=PTS-STARTPTS[a{}]",
            last_end, total_duration, segment_index
        ));
        
        if has_video {
            concat_inputs.push(format!("[v{}][a{}]", segment_index, segment_index));
        } else {
            concat_inputs.push(format!("[a{}]", segment_index));
        }
        segment_index += 1;
    }
    
    // 拼接所有片段
    if segment_index > 1 {
        // 多个片段，需要拼接
        if has_video {
            filter_parts.push(format!(
                "{}concat=n={}:v=1:a=1[v][a]",
                concat_inputs.join(""),
                segment_index
            ));
        } else {
            filter_parts.push(format!(
                "{}concat=n={}:v=0:a=1[a]",
                concat_inputs.join(""),
                segment_index
            ));
        }
    } else if segment_index == 1 {
        // 只有一个片段，直接输出
        if has_video {
            filter_parts.push("[v0]copy[v]".to_string());
            filter_parts.push("[a0]copy[a]".to_string());
        } else {
            filter_parts.push("[a0]copy[a]".to_string());
        }
    } else {
        // 没有有效片段，输出原始流
        if has_video {
            return "[0:v]copy[v];[0:a]copy[a]".to_string();
        } else {
            return "[0:a]copy[a]".to_string();
        }
    }
    
    filter_parts.join(";")
}

// 批量处理
pub async fn batch_process_videos(
    input_paths: &[String],
    output_dir: &str,
    _threshold_db: f64,
    _min_silence_duration: f64,
) -> Result<Vec<ProcessResult>, Box<dyn std::error::Error>> {
    let mut results = Vec::new();
    
    // 确保输出目录存在
    fs::create_dir_all(output_dir)?;
    
    for (index, input_path) in input_paths.iter().enumerate() {
        let output_filename = format!("processed_{}.mp4", index + 1);
        let output_path = format!("{}/{}", output_dir, output_filename);
        
        // 这里应该实际处理每个视频
        // 为了简化，先返回模拟结果
        
        results.push(ProcessResult {
            input_path: input_path.clone(),
            output_path,
            original_duration: 60.0,
            processed_duration: 50.0,
            silence_segments: 5,
            total_silence_removed: 10.0,
            compression_ratio: 16.67,
            processing_time: 2.5,
            success: true,
            error_message: None,
        });
    }
    
    Ok(results)
}