// src-tauri/src/commands/video_processing.rs
// 视频处理命令

use crate::audio;
use crate::video;
use crate::app::ExportState;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::Ordering;

// 视频处理请求
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoProcessRequest {
    pub input_path: String,
    pub output_path: Option<String>,
    pub threshold_db: f64,
    pub min_silence_duration: f64,
    pub sample_rate: Option<u32>,
    pub segments: Option<Vec<crate::audio::SilenceSegment>>,
}

// 视频处理响应
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoProcessResponse {
    pub success: bool,
    pub message: String,
    pub original_duration: f64,
    pub processed_duration: f64,
    pub silence_segments: usize,
    pub total_silence_removed: f64,
    pub compression_ratio: f64,
    pub output_path: String,
    pub processing_time: f64,
}

// 取消导出命令
#[tauri::command]
pub async fn cancel_export(state: tauri::State<'_, ExportState>) -> Result<(), String> {
    state.is_cancelled.store(true, Ordering::SeqCst);
    println!("🛑 收到取消信号，将尝试停止当前处理...");
    Ok(())
}

// 获取视频信息
#[tauri::command]
pub async fn get_video_info(
    state: tauri::State<'_, crate::app::AppState>,
    path: String
) -> Result<video::VideoInfo, String> {
    let ffprobe_path = state.ffprobe_path.as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "FFprobe not found".to_string())?;

    let result: Result<video::VideoInfo, Box<dyn std::error::Error>> = video::get_video_info(&ffprobe_path, &path).await;
    result.map_err(|e| format!("获取视频信息失败: {}", e))
}

// 提取音频 (流式分析版)
#[tauri::command]
pub async fn extract_audio(
    state: tauri::State<'_, crate::app::AppState>,
    path: String,
    sample_rate: Option<u32>,
    window: tauri::Window,
) -> Result<audio::AudioData, String> {
    let ffmpeg_path = state.ffmpeg_path.as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "FFmpeg not found".to_string())?;
    
    let ffprobe_path = state.ffprobe_path.as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "FFprobe not found".to_string())?;

    let sample_rate = sample_rate.unwrap_or(16000);
    println!("开始流式提取音频: {}, 采样率: {}", path, sample_rate);
    
    // 发送初始进度事件
    use tauri::Emitter;
    let _ = window.emit("analysis-progress", serde_json::json!({
        "stage": "extracting",
        "message": "正在流式提取音频...",
        "percent": 5
    }));
    
    // 调用我们在 audio/mod.rs 中定义的流式处理函数
    let result = audio::extract_audio_streaming(&ffmpeg_path, &ffprobe_path, &path, sample_rate, &window, -40.0).await;
    
    result.map_err(|e| {
        println!("提取音频失败: {}", e);
        format!("提取音频失败: {}", e)
    })
}

// 检测静音
#[tauri::command]
pub async fn detect_silences(
    cache_id: String,
    audio_data: Option<Vec<f32>>,
    sample_rate: u32,
    threshold_db: f64,
    min_silence_duration: f64,
    window: tauri::Window,
) -> Result<Vec<audio::SilenceSegment>, String> {
    use tauri::Emitter;
    // 发送进度事件
    let _ = window.emit("analysis-progress", serde_json::json!({
        "stage": "detecting",
        "message": "正在分析音频静音片段...",
        "percent": 60
    }));
    
    audio::detect_silences(
        &cache_id,
        audio_data.as_deref(),
        sample_rate,
        threshold_db,
        min_silence_duration,
    )
    .map_err(|e| format!("静音检测失败: {}", e))
}

// 处理视频
#[tauri::command]
pub async fn process_video(
    app_state: tauri::State<'_, crate::app::AppState>,
    request: VideoProcessRequest,
    window: tauri::Window,
    state: tauri::State<'_, ExportState>,
) -> Result<VideoProcessResponse, String> {
    let ffmpeg_path = app_state.ffmpeg_path.as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "FFmpeg not found".to_string())?;
    
    let ffprobe_path = app_state.ffprobe_path.as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "FFprobe not found".to_string())?;

    // 重置取消标记
    state.is_cancelled.store(false, Ordering::SeqCst);
    
    let start_time = std::time::Instant::now();

    // 生成输出路径
    let output_path = match request.output_path {
        Some(path) => path,
        None => generate_output_path(&request.input_path),
    };
    
    // 提取音频
    let sample_rate = request.sample_rate.unwrap_or(16000);
    println!("========== 开始视频处理 ==========");
    println!("输入文件: {}", request.input_path);
    println!("输出文件: {}", output_path);
    println!("采样率: {} Hz", sample_rate);
    println!("阈值: {} dB", request.threshold_db);
    println!("最小静音时长: {} 秒", request.min_silence_duration);
    
    // 如果前端已经提供了静音片段（常见情况），直接进入处理，跳过音频提取和重复检测
    let silences = if let Some(segs) = request.segments {
        println!("✅ 使用前端提供的静音片段，数量: {}", segs.len());
        segs
    } else {
        println!("未提供片段，开始从视频提取音频并检测...");
        let result: Result<audio::AudioData, Box<dyn std::error::Error>> = audio::extract_audio_from_video(&ffmpeg_path, &ffprobe_path, &request.input_path, sample_rate, Some(&window)).await;
        let audio_data = result.map_err(|e| {
            eprintln!("❌ 音频提取失败: {}", e);
            format!("音频提取失败: {}", e)
        })?;
        
        println!("✅ 音频提取成功, 缓存ID: {}", audio_data.cache_id);
        
        audio::detect_silences(
            &audio_data.cache_id,
            audio_data.samples.as_deref(),
            audio_data.sample_rate,
            request.threshold_db,
            request.min_silence_duration,
        )
        .map_err(|e| {
            eprintln!("❌ 静音检测失败: {}", e);
            format!("静音检测失败: {}", e)
        })?
    };
    
    println!("✅ 静音检测/获取完成: {} 个片段", silences.len());
    
    // 处理视频
    let cancel_signal = state.is_cancelled.clone();
    
    let video_result: Result<video::ProcessResult, Box<dyn std::error::Error>> = video::remove_silence_from_video(
        &ffmpeg_path,
        &ffprobe_path,
        &request.input_path,
        &output_path,
        &silences,
        Some(window),
        cancel_signal,
    ).await;
    let result = video_result.map_err(|e| {
        if e.to_string() == "EXPORT_CANCELLED" {
            return "EXPORT_CANCELLED".to_string();
        }
        format!("视频处理失败: {}", e)
    })?;
    
    let processing_time = start_time.elapsed().as_secs_f64();
    
    Ok(VideoProcessResponse {
        success: true,
        message: "视频处理完成".to_string(),
        original_duration: result.original_duration,
        processed_duration: result.processed_duration,
        silence_segments: result.silence_segments,
        total_silence_removed: result.total_silence_removed,
        compression_ratio: result.compression_ratio,
        output_path: result.output_path,
        processing_time,
    })
}

// 批量处理
#[tauri::command]
pub async fn batch_process(
    input_paths: Vec<String>,
    output_dir: String,
    threshold_db: f64,
    min_silence_duration: f64,
) -> Result<Vec<video::ProcessResult>, String> {
    let result: Result<Vec<video::ProcessResult>, Box<dyn std::error::Error>> = video::batch_process_videos(
        &input_paths,
        &output_dir,
        threshold_db,
        min_silence_duration,
    ).await;
    result.map_err(|e| format!("批量处理失败: {}", e))
}

// 生成输出路径
fn generate_output_path(input_path: &str) -> String {
    let path = Path::new(input_path);
    let parent = path.parent().unwrap_or(Path::new("."));
    let stem = path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let extension = path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("mp4");
    
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let filename = format!("{}_{}_cut.{}", stem, timestamp, extension);
    
    parent.join(filename).to_string_lossy().to_string()
}



