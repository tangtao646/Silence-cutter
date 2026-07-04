// src-tauri/src/audio/mod.rs
// 音频处理模块 - 更新版本

use serde::{Deserialize, Serialize};
use serde_json;
use std::process::Command;
use tempfile::NamedTempFile;
use std::fs;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use once_cell::sync::Lazy;

// 全局音频数据缓存，避免大数据通过 IPC 传输
static AUDIO_CACHE: Lazy<Arc<Mutex<HashMap<String, Vec<f32>>>>> = Lazy::new(|| {
    Arc::new(Mutex::new(HashMap::new()))
});

// 解析时间字符串 (HH:MM:SS.ms 或 SS.ms)
fn parse_time(time_str: &str) -> Result<f64, Box<dyn std::error::Error>> {
    let parts: Vec<&str> = time_str.split(':').collect();
    let time = match parts.len() {
        3 => {
            let hours: f64 = parts[0].parse()?;
            let minutes: f64 = parts[1].parse()?;
            let seconds: f64 = parts[2].parse()?;
            hours * 3600.0 + minutes * 60.0 + seconds
        }
        2 => {
            let minutes: f64 = parts[0].parse()?;
            let seconds: f64 = parts[1].parse()?;
            minutes * 60.0 + seconds
        }
        1 => parts[0].parse()?,
        _ => return Err("无效的时间格式".into()),
    };
    Ok(time)
}

// 获取视频时长
fn get_video_duration(ffprobe_path: &str, video_path: &str) -> Result<f64, Box<dyn std::error::Error>> {
    let output = Command::new(ffprobe_path)
        .args(&["-v", "error"])
        .args(&["-show_entries", "format=duration"])
        .args(&["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(video_path)
        .output()?;
    
    if !output.status.success() {
        return Err("获取视频时长失败".into());
    }
    
    let duration_str = String::from_utf8(output.stdout)?;
    let duration: f64 = duration_str.trim().parse()?;
    Ok(duration)
}

// 音频数据
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples: Option<Vec<f32>>,
    pub peaks: Vec<f32>,     // 关键：用于前端快速渲染的峰值数据
    pub sample_rate: u32,
    pub duration: f64,
    pub channels: u32,
    pub format: String,
    pub bit_depth: u32,
    pub cache_id: String,    // 用于后续分析的引用标识
}

// 静音片段
#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct SilenceSegment {
    pub start_time: f64,
    pub end_time: f64,
    #[serde(default)]
    pub duration: f64,
    pub average_db: f64,
}

impl SilenceSegment {
    pub fn new(start_time: f64, end_time: f64, average_db: f64) -> Self {
        Self {
            start_time,
            end_time,
            duration: end_time - start_time,
            average_db,
        }
    }
}

// ============================================================
// 增量静音检测器：随 extract_audio_streaming 流式处理同步检测
// 避免全量计算完毕后才返回结果，实现"边提取边绘制静音片段"
// ============================================================
struct IncrementalSilenceDetector {
    threshold_linear: f64,
    min_silence_samples: usize,
    window_size: usize,
    sample_rate: u32,
    last_processed: usize,
    in_silence: bool,
    silence_start: usize,
    silence_energy_sum: f64,
    silence_windows: usize,
}

impl IncrementalSilenceDetector {
    fn new(sample_rate: u32, threshold_db: f64, min_silence_duration: f64) -> Self {
        let threshold_linear = 10.0f64.powf(threshold_db / 20.0);
        let min_silence_samples = (min_silence_duration * sample_rate as f64) as usize;
        let window_size = ((sample_rate as f64) * 0.02) as usize; // 20ms 窗口
        Self {
            threshold_linear,
            min_silence_samples,
            window_size: window_size.max(1),
            sample_rate,
            last_processed: 0,
            in_silence: false,
            silence_start: 0,
            silence_energy_sum: 0.0,
            silence_windows: 0,
        }
    }

    /// 对自上次调用以来新增的样本做增量检测
    /// 返回本批新确认的完整静音片段（跨批边界未结束的静音保留状态）
    fn process_new_samples(&mut self, all_samples: &[f32]) -> Vec<SilenceSegment> {
        let mut new_silences = Vec::new();
        let available = all_samples.len();

        if available <= self.last_processed || self.window_size == 0 {
            return new_silences;
        }

        let start = self.last_processed;

        for window_start in (start..available).step_by(self.window_size) {
            let window_end = (window_start + self.window_size).min(available);
            let chunk = &all_samples[window_start..window_end];
            let energy = calculate_rms(chunk);

            if energy < self.threshold_linear {
                if !self.in_silence {
                    self.in_silence = true;
                    self.silence_start = window_start;
                    self.silence_energy_sum = energy;
                    self.silence_windows = 1;
                } else {
                    self.silence_energy_sum += energy;
                    self.silence_windows += 1;
                }
            } else if self.in_silence {
                // 静音段结束
                self.in_silence = false;
                let silence_end = window_start;
                let silence_samples = silence_end - self.silence_start;

                if silence_samples >= self.min_silence_samples {
                    let start_time = self.silence_start as f64 / self.sample_rate as f64;
                    let end_time = silence_end as f64 / self.sample_rate as f64;
                    let average_energy = self.silence_energy_sum / self.silence_windows as f64;
                    let average_db = linear_to_db(average_energy);

                    new_silences.push(SilenceSegment::new(start_time, end_time, average_db));
                }
            }
        }

        self.last_processed = available;
        new_silences
    }

    /// 最终收尾：处理末尾仍在持续的静音段
    fn finalize(&mut self, all_samples: &[f32]) -> Vec<SilenceSegment> {
        let mut final_silences = Vec::new();

        if self.in_silence {
            let silence_end = all_samples.len();
            let silence_samples = silence_end - self.silence_start;

            if silence_samples >= self.min_silence_samples {
                let start_time = self.silence_start as f64 / self.sample_rate as f64;
                let end_time = silence_end as f64 / self.sample_rate as f64;
                let average_energy = self.silence_energy_sum / self.silence_windows as f64;
                let average_db = linear_to_db(average_energy);

                final_silences.push(SilenceSegment::new(start_time, end_time, average_db));
            }
            self.in_silence = false;
        }

        final_silences
    }
}

// 从视频流式提取音频并实时分析
pub async fn extract_audio_streaming(
    ffmpeg_path: &str,
    ffprobe_path: &str,
    video_path: &str,
    sample_rate: u32,
    window: &tauri::Window,
    threshold_db: f64,
    min_silence_duration: f64,
) -> Result<AudioData, Box<dyn std::error::Error>> {
    use std::process::Stdio;
    use std::io::Read;
    use tauri::Emitter;

    let duration = get_video_duration(ffprobe_path, video_path).unwrap_or(0.0);
    
    // FFmpeg 命令：直接输出原始 PCM 数据到 stdout
    let mut child = Command::new(ffmpeg_path)
        .args(&["-i", video_path])
        .args(&["-vn"])
        .args(&["-ac", "1"])
        .args(&["-ar", &sample_rate.to_string()])
        .args(&["-f", "s16le"]) // 输出原始 16-bit 采样
        .arg("-")               // 输出到 stdout
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let mut stdout = child.stdout.take().ok_or("无法打开 ffmpeg stdout")?;
    
    let mut all_samples = Vec::new();
    let mut buffer = [0u8; 16384]; 
    let mut leftover: Vec<u8> = Vec::new();
    
    let mut peaks = Vec::new();
    let mut current_peak: f32 = 0.0;
    let mut samples_in_peak = 0;
    // 每 20ms 计算一个峰值，提升前端渲染流畅度
    let peak_window = (sample_rate / 50).max(1) as usize; 
    
    let mut total_samples = 0;
    let mut final_peaks = Vec::new();

    // 初始化增量静音检测器
    let mut silence_detector = IncrementalSilenceDetector::new(
        sample_rate,
        threshold_db,
        min_silence_duration,
    );

    // 用于记录流式过程中已经分批发送给前端的所有静音片段，以便最后合并做最终状态同步
    let mut chunk_samples = Vec::with_capacity(sample_rate as usize * 2); // 临时存放用于增量检测的样本

    loop {
        let n = match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                log::error!("读取音频流出错: {}", e);
                break;
            }
        };
        
        // 处理可能存在的残余字节
        let mut data = Vec::with_capacity(leftover.len() + n);
        data.extend_from_slice(&leftover);
        data.extend_from_slice(&buffer[..n]);
        leftover.clear();

        let mut i = 0;
        while i + 1 < data.len() {
            let s16 = i16::from_le_bytes([data[i], data[i+1]]);
            let f32_sample = s16 as f32 / 32768.0;
            
            all_samples.push(f32_sample);
            chunk_samples.push(f32_sample); // 收集当前批次的增量数据
            total_samples += 1;
            
            // 计算峰值
            current_peak = current_peak.max(f32_sample.abs());
            samples_in_peak += 1;
            
            if samples_in_peak >= peak_window {
                peaks.push(current_peak);
                final_peaks.push(current_peak);
                
                // 累计 100 个峰值 (约 2 秒) 发送一次波形和静音增量
                if peaks.len() >= 100 {
                    let mut progress = if duration > 0.0 {
                        (total_samples as f64 / sample_rate as f64) / duration
                    } else {
                        0.0
                    };
                    if progress > 1.0 { progress = 1.0; }
                    
                    // 1. 发送波形数据
                    let _ = window.emit("audio-waveform-step", serde_json::json!({
                        "peaks": peaks,
                        "progress": progress,
                    }));
                    peaks = Vec::new();

                    // 2. 真正的增量静音检测：只传入这约 2 秒内新积攒的音频样本 `chunk_samples`
                    // 这样 Detector 内部的绝对位置游标就不会乱
                    let new_silences = silence_detector.process_new_samples(&chunk_samples);
                    if !new_silences.is_empty() {
                        let _ = window.emit("silence-detect-step", serde_json::json!({
                            "segments": new_silences.iter().map(|s| {
                                serde_json::json!({
                                    "startTime": s.start_time,
                                    "endTime": s.end_time,
                                    "duration": s.duration,
                                    "averageDb": s.average_db,
                                })
                            }).collect::<Vec<_>>(),
                            "progress": progress,
                        }));
                    }
                    chunk_samples.clear(); // 发送完后清空增量池，为下一轮做准备
                }
                
                current_peak = 0.0;
                samples_in_peak = 0;
            }
            i += 2;
        }

        // 保存未处理的单字节
        if i < data.len() {
            leftover.extend_from_slice(&data[i..]);
        }
    }

    // --- 退出 Loop，开始收尾工作 ---

    // 处理流式末尾最后不足 100 个峰值遗留的 chunk_samples
    if !chunk_samples.is_empty() {
        let remaining_silences = silence_detector.process_new_samples(&chunk_samples);
        if !remaining_silences.is_empty() {
            let _ = window.emit("silence-detect-step", serde_json::json!({
                "segments": remaining_silences.iter().map(|s| {
                    serde_json::json!({
                        "startTime": s.start_time,
                        "endTime": s.end_time,
                        "duration": s.duration,
                        "averageDb": s.average_db,
                    })
                }).collect::<Vec<_>>(),
                "progress": 1.0,
            }));
        }
    }

    let actual_duration = total_samples as f64 / sample_rate as f64;
    let cache_id = video_path.to_string();

    // 针对整段音频全量跑一次最终检测，用于生成最终完美的、经过 merge_close_silences 缝合的确定版数据
    // 这样做可以完全规避流式边界处的断裂问题
    let all_detected = match detect_silences(&cache_id, Some(&all_samples), sample_rate, threshold_db, min_silence_duration) {
        Ok(res) => res,
        Err(_) => merge_close_silences(silence_detector.finalize(&all_samples), sample_rate)
    };

    // 关键步骤：存入缓存
    if let Ok(mut cache) = AUDIO_CACHE.lock() {
        cache.insert(cache_id.clone(), all_samples);
    }

    println!("流式提取完成: {}, 时长: {:.2}s, 峰值数: {}", cache_id, actual_duration, final_peaks.len());

    // 发送波形最终状态
    let _ = window.emit("audio-waveform-done", serde_json::json!({
        "duration": actual_duration,
        "totalSamples": total_samples,
        "cache_id": cache_id.clone(),
        "peaks": if final_peaks.len() > 500000 { Vec::<f32>::new() } else { final_peaks.clone() }
    }));

    // 发送流式静音检测最终确定事件（前端收到此事件后，用最终的完整 Array 覆盖之前的流式追加缓存，体验最稳健）
    println!("🔊 流式静音检测最终同步: {} 个静音片段", all_detected.len());
    let _ = window.emit("silence-detection-done", serde_json::json!({
        "segments": all_detected.iter().map(|s| {
            serde_json::json!({
                "startTime": s.start_time,
                "endTime": s.end_time,
                "duration": s.duration,
                "averageDb": s.average_db,
            })
        }).collect::<Vec<_>>(),
        "duration": actual_duration,
        "totalSegments": all_detected.len(),
    }));

    Ok(AudioData {
        samples: None, 
        peaks: final_peaks,
        sample_rate,
        duration: actual_duration,
        channels: 1,
        format: "s16le".to_string(),
        cache_id,
        bit_depth: 16,
    })
}

// 从视频提取音频
pub async fn extract_audio_from_video(
    ffmpeg_path: &str,
    ffprobe_path: &str,
    video_path: &str,
    sample_rate: u32,
    window: Option<&tauri::Window>,
) -> Result<AudioData, Box<dyn std::error::Error>> {
    use std::process::Stdio;
    use std::io::{BufRead, BufReader};
    use regex::Regex;
    use tauri::Emitter;
    
    // 创建临时文件
    let temp_wav = NamedTempFile::new()?;
    let temp_wav_path = temp_wav.path().to_str().unwrap();
    
    println!("提取音频到临时文件: {}", temp_wav_path);
    
    // 先获取视频时长
    let duration = get_video_duration(ffprobe_path, video_path)?;
    println!("视频时长: {:.2}s", duration);
    
    // 使用 ffmpeg 提取音频为 WAV（带进度输出）
    let mut child = Command::new(ffmpeg_path)
        .args(&["-i", video_path])
        .args(&["-vn"])                     // 无视频
        .args(&["-ac", "1"])                // 单声道
        .args(&["-ar", &sample_rate.to_string()]) // 采样率
        .args(&["-acodec", "pcm_s16le"])    // 16-bit PCM
        .args(&["-f", "wav"])               // WAV 格式
        .args(&["-progress", "pipe:2"])     // 进度输出到 stderr
        .args(&["-y"])                      // 覆盖输出
        .arg(temp_wav_path)
        .stderr(Stdio::piped())
        .spawn()?;
    
    // 解析进度
    if let Some(stderr) = child.stderr.take() {
        let reader = BufReader::new(stderr);
        let time_regex = Regex::new(r"time=([0-9:.]+)").unwrap();
        let speed_regex = Regex::new(r"speed=([0-9.]+)x").unwrap();
        
        for line in reader.lines() {
            if let Ok(line) = line {
                // 解析当前时间
                if let Some(time_cap) = time_regex.captures(&line) {
                    if let Some(time_str) = time_cap.get(1) {
                        if let Ok(current_time) = parse_time(time_str.as_str()) {
                            let percent = ((current_time / duration) * 100.0).min(95.0);
                            
                            // 计算 ETA
                            let speed = speed_regex.captures(&line)
                                .and_then(|cap| cap.get(1))
                                .and_then(|s| s.as_str().parse::<f64>().ok())
                                .unwrap_or(1.0);
                            
                            let remaining_time = (duration - current_time) / speed.max(0.01);
                            
                            let eta_min = (remaining_time / 60.0).floor() as i32;
                            let eta_sec = (remaining_time % 60.0).floor() as i32;
                            let eta_text = if eta_min > 0 {
                                format!("{}分{}秒", eta_min, eta_sec)
                            } else {
                                format!("{}秒", eta_sec)
                            };
                            
                            // 发送进度事件
                            if let Some(win) = window {
                                let _ = win.emit("analysis-progress", serde_json::json!({
                                    "message": format!("正在提取音频... 速度: {:.2}x, 预计剩余: {}", speed, eta_text),
                                    "percent": percent
                                }));
                            }
                        }
                    }
                }
            }
        }
    }
    
    let output = child.wait_with_output()?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("FFmpeg 音频提取失败: {}", stderr);
        return Err("FFmpeg 音频提取失败".into());
    }
    
    println!("音频提取成功");
    
    // 获取音频信息
    let duration = get_audio_duration(ffprobe_path, temp_wav_path)?;
    let file_size = fs::metadata(temp_wav_path)?.len();
    
    println!("音频时长: {:.2}s, 文件大小: {} bytes", duration, file_size);
    
    // 读取 WAV 文件数据
    let samples = read_wav_file(temp_wav_path, sample_rate)?;
    
    println!("读取了 {} 个样本", samples.len());
    
    // 存入缓存
    let cache_id = video_path.to_string();
    if let Ok(mut cache) = AUDIO_CACHE.lock() {
        cache.insert(cache_id.clone(), samples.clone());
    }

    // 计算快速预览峰值 (每 20ms 一个)
    let peak_window = (sample_rate / 50) as usize;
    let peaks: Vec<f32> = samples.chunks(peak_window)
        .map(|chunk| chunk.iter().fold(0.0f32, |max, &s| max.max(s.abs())))
        .collect();

    Ok(AudioData {
        samples: None,
        peaks,
        sample_rate,
        duration,
        channels: 1,
        format: "WAV".to_string(),
        cache_id,
        bit_depth: 16,
    })
}

// 读取 WAV 文件
fn read_wav_file(wav_path: &str, expected_sample_rate: u32) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    use std::io::{Read, Seek, SeekFrom};
    
    let mut file = fs::File::open(wav_path)?;
    
    // 读取 WAV 头
    let mut header = [0u8; 12];
    file.read_exact(&mut header)?;
    
    // 验证 RIFF 和 WAVE 标识
    if &header[0..4] != b"RIFF" {
        return Err("不是有效的 RIFF 文件".into());
    }
    if &header[8..12] != b"WAVE" {
        return Err("不是有效的 WAVE 文件".into());
    }
    
    let file_size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    println!("RIFF 文件大小: {} bytes", file_size);
    
    // 读取所有 chunks，找到 fmt 和 data
    let mut audio_format = 0u16;
    let mut num_channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut data_offset = 0u64;
    let mut data_size = 0u32;
    
    let mut current_pos = 12u64;
    
    while current_pos < (file_size as u64 + 8) {
        file.seek(SeekFrom::Start(current_pos))?;
        
        let mut chunk_header = [0u8; 8];
        if let Err(_) = file.read_exact(&mut chunk_header) {
            break; // 文件结束
        }
        
        let chunk_id = &chunk_header[0..4];
        let chunk_size = u32::from_le_bytes([chunk_header[4], chunk_header[5], chunk_header[6], chunk_header[7]]);
        
        println!("找到 chunk: {:?}, 大小: {} bytes", 
                 std::str::from_utf8(chunk_id).unwrap_or("???"), chunk_size);
        
        if chunk_id == b"fmt " {
            // 读取 fmt chunk
            let mut fmt_data = vec![0u8; chunk_size as usize];
            file.read_exact(&mut fmt_data)?;
            
            audio_format = u16::from_le_bytes([fmt_data[0], fmt_data[1]]);
            num_channels = u16::from_le_bytes([fmt_data[2], fmt_data[3]]);
            sample_rate = u32::from_le_bytes([fmt_data[4], fmt_data[5], fmt_data[6], fmt_data[7]]);
            bits_per_sample = u16::from_le_bytes([fmt_data[14], fmt_data[15]]);
            
            println!("WAV 格式: 采样率={}Hz, 声道={}, 位深={}", sample_rate, num_channels, bits_per_sample);
        } else if chunk_id == b"data" {
            // 找到 data chunk
            data_offset = current_pos + 8;
            data_size = chunk_size;
            println!("找到 data chunk，偏移: {}, 大小: {} bytes", data_offset, data_size);
        }
        
        // 移动到下一个 chunk (chunk 大小可能是奇数，需要对齐到偶数)
        let aligned_chunk_size = if chunk_size % 2 == 1 { chunk_size + 1 } else { chunk_size };
        current_pos += 8 + aligned_chunk_size as u64;
    }
    
    if data_size == 0 {
        return Err("找不到 data chunk".into());
    }
    
    if bits_per_sample != 16 {
        return Err(format!("不支持的位深: {}", bits_per_sample).into());
    }
    
    // 读取音频数据
    file.seek(SeekFrom::Start(data_offset))?;
    let mut pcm_data = vec![0u8; data_size as usize];
    file.read_exact(&mut pcm_data)?;
    
    // 转换为 f32 样本
    let num_samples = data_size as usize / (bits_per_sample as usize / 8) / (num_channels as usize);
    let mut samples = Vec::with_capacity(num_samples);
    
    // 16-bit PCM
    for i in 0..num_samples {
        let offset = i * num_channels as usize * 2;
        let sample_i16 = i16::from_le_bytes([
            pcm_data[offset],
            pcm_data[offset + 1]
        ]);
        
        // 归一化到 [-1.0, 1.0]
        let sample_f32 = sample_i16 as f32 / 32768.0;
        samples.push(sample_f32);
    }
    
    Ok(samples)
}

// 获取音频时长
fn get_audio_duration(ffprobe_path: &str, wav_path: &str) -> Result<f64, Box<dyn std::error::Error>> {
    let output = Command::new(ffprobe_path)
        .args(&["-v", "error"])
        .args(&["-show_entries", "format=duration"])
        .args(&["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(wav_path)
        .output()?;
    
    let output_str = String::from_utf8_lossy(&output.stdout);
    output_str.trim().parse::<f64>()
        .map_err(|e| format!("解析音频时长失败: {}", e).into())
}

// 修改检测函数，支持从缓存读取
pub fn detect_silences(
    cache_id: &str,
    audio_data_fallback: Option<&[f32]>,
    sample_rate: u32,
    threshold_db: f64,
    min_silence_duration: f64,
) -> Result<Vec<SilenceSegment>, Box<dyn std::error::Error>> {
    // 优先从缓存获取数据
    let cache = AUDIO_CACHE.lock().unwrap();
    let audio_data = if let Some(data) = cache.get(cache_id) {
        println!("使用缓存数据进行分析: {}", cache_id);
        data
    } else if let Some(fallback) = audio_data_fallback {
        println!("由于缓存失效，使用传输的备选数据进行分析");
        fallback
    } else {
        return Err("无法获取音频数据：缓存已失效且未提供备选数据".into());
    };

    // 原有的检测逻辑（保持不变）
    if audio_data.is_empty() {
        println!("警告: 音频数据为空");
        return Ok(Vec::new());
    }
    
    if min_silence_duration <= 0.0 {
        return Err("最小静音时长必须大于0".into());
    }
    
    println!("============================================");
    println!("开始静音检测 (ID: {})", cache_id);
    println!("样本数: {}", audio_data.len());
    println!("采样率: {} Hz", sample_rate);
    println!("阈值: {} dB", threshold_db);
    println!("最小静音时长: {} 秒", min_silence_duration);
    
    // 转换为分贝阈值
    let threshold_linear = db_to_linear(threshold_db);
    let min_silence_samples = (min_silence_duration * sample_rate as f64) as usize;
    let window_size = (sample_rate as f64 * 0.02) as usize; // 20ms窗口
    
    println!("线性阈值: {:.6}", threshold_linear);
    println!("最小静音样本数: {}", min_silence_samples);
    println!("窗口大小: {} 样本", window_size);
    
    if window_size == 0 {
        return Err("采样率太低".into());
    }
    
    let mut silences = Vec::new();
    let mut in_silence = false;
    let mut silence_start = 0;
    let mut silence_energy_sum = 0.0;
    let mut silence_windows = 0;
    
    let mut total_windows = 0;
    let mut silent_window_count = 0;
    
    // 滑动窗口检测
    for (window_index, chunk) in audio_data.chunks(window_size).enumerate() {
        let energy = calculate_rms(chunk);
        total_windows += 1;
        
        // 每 5000 个窗口打印一次状态，减少日志
        if window_index % 5000 == 0 {
            let energy_db = linear_to_db(energy);
            println!("分析进度 {}%: 能量={:.2} dB", (window_index * 100 / total_windows.max(1)), energy_db);
        }
        
        if energy < threshold_linear {
            silent_window_count += 1;
            // 当前窗口是静音
            if !in_silence {
                // 开始新的静音段
                in_silence = true;
                silence_start = window_index * window_size;
                silence_energy_sum = energy;
                silence_windows = 1;
            } else {
                // 继续静音段
                silence_energy_sum += energy;
                silence_windows += 1;
            }
        } else {
            // 当前窗口不是静音
            if in_silence {
                // 静音段结束
                in_silence = false;
                let silence_end = window_index * window_size;
                let silence_samples = silence_end - silence_start;
                
                if silence_samples >= min_silence_samples {
                    let start_time = silence_start as f64 / sample_rate as f64;
                    let end_time = silence_end as f64 / sample_rate as f64;
                    let average_energy = silence_energy_sum / silence_windows as f64;
                    let average_db = linear_to_db(average_energy);
                    
                    silences.push(SilenceSegment::new(
                        start_time,
                        end_time,
                        average_db,
                    ));
                }
            }
        }
    }
    
    // 处理最后一段静音
    if in_silence {
        let silence_end = audio_data.len();
        let silence_samples = silence_end - silence_start;
        
        if silence_samples >= min_silence_samples {
            let start_time = silence_start as f64 / sample_rate as f64;
            let end_time = silence_end as f64 / sample_rate as f64;
            let average_energy = silence_energy_sum / silence_windows as f64;
            let average_db = linear_to_db(average_energy);
            
            silences.push(SilenceSegment::new(
                start_time,
                end_time,
                average_db,
            ));
        }
    }
    
    println!("分析完成！合并片段中...");
    
    // 合并相邻静音段
    let merged = merge_close_silences(silences, sample_rate);
    println!("分析结果: 检测到 {} 个静音片段", merged.len());
    println!("============================================");
    
    Ok(merged)
}

// 计算 RMS
fn calculate_rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    
    let sum: f64 = samples.iter()
        .map(|&x| (x as f64).powi(2))
        .sum();
    
    (sum / samples.len() as f64).sqrt()
}

// 线性到分贝
fn linear_to_db(linear: f64) -> f64 {
    if linear <= 0.0 {
        -100.0
    } else {
        20.0 * linear.log10()
    }
}

// 分贝到线性
fn db_to_linear(db: f64) -> f64 {
    10.0f64.powf(db / 20.0)
}

// 合并相邻静音
fn merge_close_silences(
    mut silences: Vec<SilenceSegment>,
    sample_rate: u32,
) -> Vec<SilenceSegment> {
    if silences.len() < 2 {
        return silences;
    }
    
    silences.sort_by(|a, b| a.start_time.partial_cmp(&b.start_time).unwrap());
    
    let mut merged = Vec::new();
    let gap_threshold = 0.1; // 100ms间隔内合并
    
    let mut current = silences[0];
    
    for next in silences.iter().skip(1) {
        if next.start_time - current.end_time <= gap_threshold {
            // 合并：计算加权平均分贝值
            let total_duration = current.duration + next.duration;
            let weight_current = current.duration / total_duration;
            let weight_next = next.duration / total_duration;
            let merged_db = current.average_db * weight_current + next.average_db * weight_next;
            
            current.end_time = next.end_time;
            current.duration = current.end_time - current.start_time;
            current.average_db = merged_db;
        } else {
            merged.push(current);
            current = *next;
        }
    }
    
    merged.push(current);
    merged
}

// 音频统计
#[derive(Debug, Serialize, Deserialize)]
pub struct AudioStatistics {
    pub sample_count: usize,
    pub duration: f64,
    pub min_value: f32,
    pub max_value: f32,
    pub rms_db: f64,
    pub peak_db: f64,
    pub dynamic_range_db: f64,
    pub silence_ratio: f64,
    pub detected_silences: usize,
}

// 计算音频统计
pub fn calculate_statistics(
    audio_data: &[f32],
    sample_rate: u32,
) -> AudioStatistics {
    if audio_data.is_empty() {
        return AudioStatistics {
            sample_count: 0,
            duration: 0.0,
            min_value: 0.0,
            max_value: 0.0,
            rms_db: -100.0,
            peak_db: -100.0,
            dynamic_range_db: 0.0,
            silence_ratio: 0.0,
            detected_silences: 0,
        };
    }
    
    let sample_count = audio_data.len();
    let duration = sample_count as f64 / sample_rate as f64;
    
    // 找到最小最大值
    let (min, max) = audio_data.iter()
        .fold((f32::MAX, f32::MIN), |(min, max), &x| {
            (min.min(x), max.max(x))
        });
    
    // 计算 RMS 和峰值
    let rms = calculate_rms(audio_data);
    let rms_db = linear_to_db(rms);
    
    let peak = max.abs().max(min.abs()) as f64;
    let peak_db = linear_to_db(peak);
    
    let dynamic_range_db = peak_db - rms_db;
    
    // 估算静音比例（能量低于 -40dB 视为静音）
    let silence_threshold = db_to_linear(-40.0);
    let window_size = (sample_rate as f64 * 0.02) as usize;
    let mut silent_windows = 0;
    let total_windows = (sample_count + window_size - 1) / window_size;
    
    for chunk in audio_data.chunks(window_size) {
        let energy = calculate_rms(chunk);
        if energy < silence_threshold {
            silent_windows += 1;
        }
    }
    
    let silence_ratio = silent_windows as f64 / total_windows as f64;
    
    AudioStatistics {
        sample_count,
        duration,
        min_value: min,
        max_value: max,
        rms_db,
        peak_db,
        dynamic_range_db,
        silence_ratio,
        detected_silences: 0,
    }
}