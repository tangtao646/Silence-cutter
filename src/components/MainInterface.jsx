import React, { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import { Panel, PanelGroup, PanelResizeHandle } from 'react-resizable-panels';
import { message } from '@tauri-apps/plugin-dialog';
import LeftPanel from './LeftPanel';
import RightPanel from './RightPanel';
import WaveformSection from './WaveformSection';
import ExportProgressModal from './ExportProgressModal';
import { formatDuration, formatFileSize } from '../modules/utils';
import { useTimelineModel } from '../hooks/useTimelineModel';
import { mergeSegments, subtractSegments } from '../modules/timeline-logic';
import { useTranslation } from '../modules/i18n.jsx';

const MainInterface = ({ appData, isTauri }) => {
    const { t, language, setLanguage } = useTranslation();
    console.log('[MainInterface] Render', { hasFile: !!appData?.state?.currentFile });
    const [currentFile, setCurrentFile] = useState(null);
    const [fileInfo, setFileInfo] = useState({
        name: '--',
        size: '--',
        duration: '--:--',
        format: '--',
        hasVideo: true,
        hasAudio: true
    });
    // Guard against NaN init
    const [videoDuration, setVideoDuration] = useState(() => {
        const d = (appData?.state?.audioData?.duration) || 0;
        return Number.isFinite(d) ? d : 0;
    }); // 数值型原始时长
    const [confirmedSegments, setConfirmedSegments] = useState([]); // 基准：已确认剪掉的部分
    const [pendingSegments, setPendingSegments] = useState([]); // 动态：当前按钮探测出的拟剪掉部分
    const [viewMode, setViewMode] = useState('continuous'); // 'continuous' (overlays) or 'fragmented' (cut)
    const [waveInfo, setWaveInfo] = useState(t('status.waiting'));
    const [intensity, setIntensity] = useState(0.25); // 数值化：0.0 (None) 到 1.0 (Super)
    const [committedIntensity, setCommittedIntensity] = useState(0); // 已固化的强度基准
    const [history, setHistory] = useState([]); // 撤销历史：存储 { confirmedSegments, committedIntensity }
    const [threshold, setThreshold] = useState(-36.0); // 单位：dB，默认 -36dB (行业标准推荐值)
    const [isAutoThreshold, setIsAutoThreshold] = useState(true); // 新增：是否处于自动阈值模式
    const [padding, setPadding] = useState(0.25); // Speech Padding (Humanization)
    const [exportEnabled, setExportEnabled] = useState(false);
    const [isExporting, setIsExporting] = useState(false);
    const [exportProgress, setExportProgress] = useState(0);
    const [exportMessage, setExportMessage] = useState('');
    const [exportEta, setExportEta] = useState(0);
    const [timeDisplay, setTimeDisplay] = useState(['15m', '30m', '45m']);
    const [audioDataReady, setAudioDataReady] = useState(0); 

    // 流式静音检测：存储 extract_audio_streaming 中同步产出的增量静音片段
    const streamedSegmentsRef = useRef(null);
    const [streamingProgress, setStreamingProgress] = useState(0);

    // --- 核心状态变更与历史记录 (Undo) ---
    const commitSegments = useCallback((newSegments, skipHistory = false) => {
        if (!skipHistory) {
            // 保存当前状态到历史记录
            setHistory(prev => {
                const last = prev[prev.length - 1];
                // 如果当前状态和最后一次历史一致，不重复压栈
                if (last && JSON.stringify(last.confirmedSegments) === JSON.stringify(confirmedSegments)) {
                    return prev;
                }
                return [...prev, {
                    confirmedSegments: [...confirmedSegments],
                    committedIntensity: committedIntensity
                }].slice(-30);
            });
        }
        
        setConfirmedSegments(newSegments);
        setExportEnabled(true);
    }, [confirmedSegments, committedIntensity]);

    // 使用中台 Hook 管理核心逻辑
    const timeline = useTimelineModel({
        totalDuration: videoDuration || (appData?.state?.audioData?.duration) || 0,
        confirmedSegments,
        setConfirmedSegments: commitSegments, // 核心重构：注入带历史功能的 commit 函数
        pendingSegments,
        setPendingSegments,
        viewMode
    });

    const { stats, speechClips, mergedSilences } = timeline;

    const handleUndo = useCallback(() => {
        if (history.length === 0) {
            setWaveInfo('没有可撤销的操作');
            return;
        }
        const lastSnapshot = history[history.length - 1];
        
        // 恢复状态
        setConfirmedSegments(lastSnapshot.confirmedSegments);
        setCommittedIntensity(lastSnapshot.committedIntensity);
        setIntensity(lastSnapshot.committedIntensity); 
        
        setHistory(prev => prev.slice(0, -1));
        setWaveInfo('已撤销上一步操作');
    }, [history]);

    // 监听全局快捷键 Cmd/Ctrl + Z
    useEffect(() => {
        const handleKeyDown = (e) => {
            // 排除输入框，避免干扰正常的文本输入撤销
            if (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA') return;

            if ((e.metaKey || e.ctrlKey) && e.key === 'z') {
                e.preventDefault();
                handleUndo();
            }
        };
        window.addEventListener('keydown', handleKeyDown);
        return () => window.removeEventListener('keydown', handleKeyDown);
    }, [handleUndo]);

    // 重要：同步 confirmedSegments 状态到全局单例 appData.state
    // 确保导出模块和后台逻辑始终能拿到最新的“已确认”剪辑
    useEffect(() => {
        if (appData?.state) {
            appData.state.silenceSegments = confirmedSegments.map(s => ({
                startTime: s.start,
                endTime: s.end,
                duration: s.end - s.start,
                averageDb: s.averageDb || -60.0
            }));
        }
    }, [confirmedSegments, appData]);

    const handleUpdateSegments = useCallback((newSegments) => {
        // 用户手动调整（拖拽等）
        commitSegments(newSegments);
    }, [commitSegments]);

    // 同步时间显示
    useEffect(() => {
        const remaining = stats.remaining;
        setTimeDisplay([
            `${Math.floor(remaining * 0.25 / 60)}m`,
            `${Math.floor(remaining * 0.5 / 60)}m`,
            `${Math.floor(remaining * 0.75 / 60)}m`
        ]);
    }, [stats.remaining]);

    // 文件格式辅助函数
    const getFileFormat = (filename) => {
        if (!filename) return '未知';
        const parts = filename.split('.');
        return parts.length > 1 ? parts.pop().toUpperCase() : '未知';
    };

    // ==========================================
    // 1. 修改后的 handleAnalyze 方法
    // ==========================================
    const handleAnalyze = useCallback(async (overrides = {}) => {
        const targetIntensity = overrides.intensity !== undefined ? overrides.intensity : intensity;
        
        if (!currentFile || !appData.state.audioData) {
            return;
        }

        if (targetIntensity === 0) {
            setPendingSegments([]);
            setWaveInfo('Selected Strategy: None (No processing)');
            return;
        }

        let minSilenceDuration = 0.8;
        let targetPadding = 0.25;

        if (targetIntensity <= 0.25) {
            const ratio = targetIntensity / 0.25;
            minSilenceDuration = 3.0 - (2.2 * ratio);
            targetPadding = 0.5 - (0.25 * ratio);
        } else if (targetIntensity <= 0.5) {
            const ratio = (targetIntensity - 0.25) / 0.25;
            minSilenceDuration = 0.8 - (0.3 * ratio);
            targetPadding = 0.25 - (0.1 * ratio);
        } else {
            const ratio = (targetIntensity - 0.5) / 0.5;
            minSilenceDuration = 0.5 - (0.3 * ratio);
            targetPadding = 0.15 - (0.1 * ratio);
        }

        setPadding(targetPadding);

        console.log(`[MainInterface] Re-analyzing with params: Intensity=${targetIntensity.toFixed(2)}, minDur=${minSilenceDuration.toFixed(2)}s, threshold=${threshold}dB`);
        setWaveInfo('正在重新分析静音...');
        
        try {
            const thresholdDb = threshold;
            const audioData = appData.state.audioData;
            
            // 如果是在重新调节滑块（非首次流式处理），一律调用后端的全量检测接口
            const rawSilences = await appData.tauri.detect_silences_with_params({
                cache_id: audioData.cache_id || currentFile.path,
                threshold_db: thresholdDb,
                min_silence_duration: minSilenceDuration,
                sample_rate: audioData.sample_rate
            });

            // 统一转换逻辑的辅助函数
            const processAndMap = (silences) => {
                return silences
                    .map(s => {
                        const isAtStart = s.startTime < 0.1;
                        const isAtEnd = s.endTime > (videoDuration - 0.1);
                        const newStart = isAtStart ? 0 : s.startTime + targetPadding;
                        const newEnd = isAtEnd ? videoDuration : s.endTime - targetPadding;
                        return {
                            start: newStart,
                            end: newEnd,
                            duration: Math.max(0, newEnd - newStart),
                            averageDb: s.averageDb,
                            raw: s
                        };
                    })
                    .filter(s => s.duration > 0.05);
            };

            const mappedSegments = processAndMap(rawSilences);
            const newOnly = subtractSegments(mappedSegments, confirmedSegments);
            
            setPendingSegments(newOnly);
            setExportEnabled(true);
            setWaveInfo(`当前探测到 ${mappedSegments.length} 个建议剪减区`);
        } catch (error) {
            console.error('Analysis failed:', error);
            setWaveInfo('分析失败');
        }
    }, [currentFile, intensity, threshold, padding, videoDuration, appData, confirmedSegments]);


    // ==========================================
    // 2. 修改后的流式事件监听 Effect
    // ==========================================
    useEffect(() => {
        if (!appData.tauri || !appData.tauri.listen) return;

        let unlistenStep = null;
        let unlistenDone = null;
        let cancelled = false;

        // 根据当前的 padding 动态转换增量片段
        const convertRawToSegment = (s) => {
            const isAtStart = s.startTime < 0.1;
            const isAtEnd = s.endTime > (videoDuration - 0.1);
            const newStart = isAtStart ? 0 : s.startTime + padding;
            const newEnd = isAtEnd ? videoDuration : s.endTime - padding;
            return {
                start: newStart,
                end: newEnd,
                duration: Math.max(0, newEnd - newStart),
                averageDb: s.averageDb,
                raw: s
            };
        };

        const setup = async () => {
            // 增量事件：将后端抛出的增量静音直接转化为前端能够渲染的 pendingSegments
            unlistenStep = await appData.tauri.listen('silence-detect-step', (event) => {
                if (cancelled) return;
                const { segments, progress } = event.payload || {};
                
                if (segments && segments.length > 0) {
                    // 转换后端发来的原始数据
                    const newSegments = segments
                        .map(convertRawToSegment)
                        .filter(s => s.duration > 0.05);

                    // 实时追加到 pending 探测层，从而让前端波形图实时画出红框/色块！
                    setPendingSegments(prev => {
                        const combined = [...prev, ...newSegments];
                        // 实时排除掉已经被用户“确认切除”的部分
                        return subtractSegments(combined, confirmedSegments);
                    });
                }
                
                if (progress !== undefined) {
                    setStreamingProgress(progress);
                    setWaveInfo(`正在流式检测静音片段... ${(progress * 100).toFixed(0)}%`);
                }
            });

            // 完成事件：全量最终数据落盘校准
            unlistenDone = await appData.tauri.listen('silence-detection-done', (event) => {
                if (cancelled) return;
                const { segments, totalSegments } = event.payload || {};
                
                if (segments) {
                    const finalSegments = segments
                        .map(convertRawToSegment)
                        .filter(s => s.duration > 0.05);
                    
                    // 用最终精准的闭环合并数据覆盖流式期间杂乱的追加数据
                    const newOnly = subtractSegments(finalSegments, confirmedSegments);
                    setPendingSegments(newOnly);
                }

                setStreamingProgress(1.0);
                setExportEnabled(true);
                setWaveInfo(`静音检测完成！共找到 ${totalSegments ?? 0} 个建议剪减区`);
                console.log(`[MainInterface] Streaming silence detection done: ${totalSegments} segments`);
            });
        };

        setup();
        return () => {
            cancelled = true;
            if (unlistenStep) unlistenStep();
            if (unlistenDone) unlistenDone();
        };
    }, [appData.tauri, padding, videoDuration, confirmedSegments]);


    // ==========================================
    // 3. 修改自动化滑块检测 Effect（加上流式保护锁）
    // ==========================================
    useEffect(() => {
        // 关键改动：如果 streamingProgress 处于 0 ~ 1.0 之间（意味着正在流式提取中）
        // 强行拦截并短路防抖函数，防止流式结束的瞬间触发 handleAnalyze 导致数据重置
        if (streamingProgress > 0 && streamingProgress < 1.0) {
            return;
        }

        if (currentFile && appData.state.audioData) {
            const timer = setTimeout(() => {
                handleAnalyze();
            }, 400); 
            return () => clearTimeout(timer);
        }
    }, [threshold, padding, intensity, currentFile?.path, audioDataReady, streamingProgress, handleAnalyze]);

    const handleFileSelect = async (info) => {
        console.log('[MainInterface] File selected:', info);
        // 先丰富 info 对象
        if (isTauri && info.path && !info.path.startsWith('blob:') && !info.path.startsWith('http')) {
            const assetUrl = appData.tauri.getFileSrc(info.path);
            appData.state.currentVideoPath = info.path;
            info.previewPath = assetUrl;
            console.log('[MainInterface] Set previewPath to custom source:', assetUrl);
        } else if (info.path && (info.path.startsWith('blob:') || info.path.startsWith('http'))) {
            info.previewPath = info.path;
        }

        // 确定 previewPath 之后立即设置状态，确保预览组件能拿到有效的 src，不等待音频分析
        // 关键修复：先 set state，再 await 处理音频
        setCurrentFile({ ...info });
        appData.state.currentFile = info;
        
        // 每次导入新素材，强制重置所有页面状态到初始值
        setConfirmedSegments([]);
        setPendingSegments([]);
        setHistory([]);
        setIntensity(0.25);
        setCommittedIntensity(0);
        setThreshold(-36.0);
        setIsAutoThreshold(true);
        setPadding(0.25);
        setViewMode('continuous');
        setExportEnabled(false);
        setExportProgress(0);
        setVideoDuration(0);
        // 清空流式静音检测结果，防止跨文件污染
        streamedSegmentsRef.current = null;
        setStreamingProgress(0);

        const isAudioOnly = /\.(mp3|wav|m4a|flac|aac|ogg)$/i.test(info.name);

        setFileInfo({
            name: info.name,
            size: info.file ? formatFileSize(info.file.size) : '--',
            duration: '--:--',
            format: getFileFormat(info.name),
            hasVideo: !isAudioOnly,
            hasAudio: true
        });

        // 立即尝试获取视频时长（通过 Preview 组件回调或者 Tauri 调用）
        if (isTauri && info.path && !info.path.startsWith('blob:')) {
            appData.tauri.invoke('get_video_info', { path: info.path })
                .then(videoInfo => {
                    if (videoInfo) {
                       const safeDuration = Number.isFinite(videoInfo.duration) ? videoInfo.duration : 0;
                       setVideoDuration(safeDuration);
                       setFileInfo(prev => ({
                           ...prev,
                           duration: formatDuration(safeDuration),
                           hasVideo: !!videoInfo.has_video,
                           hasAudio: !!videoInfo.has_audio
                       }));
                    }
                })
                .catch(e => console.warn('Fast video info fetch failed', e));
        }

        // 异步开始音频提取，不阻塞界面显示
        setWaveInfo('正在提取音频...');
        requestAnimationFrame(async () => {
             try {
                await extractAudio(info);
            } catch (error) {
                console.error('Audio extraction failed:', error);
                setWaveInfo('提取失败');
            }
        });
    };

    const extractAudio = async (info) => {
        let backendPath = info.path;
        
        if (info.file && isTauri) {
            backendPath = await appData.uploader.startUploadFile(info.file, (progress) => {
                setWaveInfo(`上传中... ${(progress * 100).toFixed(0)}%`);
            });
            if (backendPath) {
                const assetUrl = appData.tauri.getFileSrc(backendPath);
                info.previewPath = assetUrl;
                appData.state.currentVideoPath = backendPath;
                setCurrentFile({ ...info });
            }
        }
        
        if (!backendPath && info.file) {
            backendPath = URL.createObjectURL(info.file);
            info.previewPath = backendPath;
            setCurrentFile({ ...info });
        }

        if (!backendPath) return;

        const audioData = await appData.tauri.extractAudio(backendPath);
        if (audioData && (audioData.peaks || audioData.cache_id)) {
            appData.state.audioData = audioData;
            setWaveInfo('音频已提取');
            
            // 重要：同步数值时长到中台 Hook
            // 注意：如果音频提取出的时长为 0（静音视频），则不覆盖已通过 ffprobe 获取的时长
            if (Number.isFinite(audioData.duration) && audioData.duration > 0) {
                setVideoDuration(audioData.duration);
                setFileInfo(prev => ({
                    ...prev,
                    duration: formatDuration(audioData.duration)
                }));
            }
            
            setThreshold(-36.0);
            setIsAutoThreshold(true);
            
            setAudioDataReady(prev => prev + 1);
        }
    };

    const handleRemoveSilence = useCallback(() => {
        if (pendingSegments.length === 0) {
            setWaveInfo('当前没有新的建议剪减区域');
            return;
        }

        // 保存当前状态到历史记录，以便撤销
        setHistory(prev => [...prev, { 
            confirmedSegments: [...confirmedSegments], 
            committedIntensity: committedIntensity 
        }]);

        // 将新的探测结果合并进基准
        const newBaseline = mergeSegments([...confirmedSegments, ...pendingSegments]);
        setConfirmedSegments(newBaseline);
        setCommittedIntensity(intensity); // 记录当前进度为已固化档位
        setPendingSegments([]); // 清空探测层
        setViewMode('fragmented'); // 自动切换到“切片视图”以显示结果
        
        setWaveInfo('已将探测结果应用到基准，剪辑已生效。您可以切换回连续模式查看更多区域');
    }, [pendingSegments, confirmedSegments, committedIntensity, intensity]);

    const handleExport = async () => {
        console.log('[MainInterface] handleExport clicked', { currentFile, segmentsCount: confirmedSegments.length });
        if (!currentFile || confirmedSegments.length === 0) {
            console.warn('[MainInterface] handleExport aborted: No file or segments');
            return;
        }


        setIsExporting(true);
        setExportProgress(0);
        setExportMessage(t('export.initializing'));
        setWaveInfo(t('export.title'));
        
        try {
            // Determine the actual path the backend should use
            const inputPath = appData.state.currentVideoPath || currentFile.path;
            
            // 导出逻辑：
            // 必须要发送“合并且排序后”的静音区间 (mergedSilences)
            // 因为 Rust 后端会基于这些区间计算“需要保留的语音片段”
            // 如果发送未排序的 confirmedSegments，后端处理逻辑会出错
            const request = {
                inputPath: inputPath,
                thresholdDb: threshold, // 已经是 dB 单位
                minSilenceDuration: 0.8, 
                segments: mergedSilences.map(s => ({
                    startTime: s.start,
                    endTime: s.end,
                    duration: s.end - s.start,
                    averageDb: s.averageDb || -60.0
                }))
            };

            const result = await appData.tauri.processVideo(request);
            console.log('Export result:', result);
            if (result && result.success) {
                setWaveInfo(t('export.completed'));
                setIsExporting(false);
                
                await message(t('export.exported_to', { path: result.outputPath }), { 
                    title: t('dialog.success')
                });
                
                // 成功关闭对话框后，自动打开所在文件夹并选中文件 (reveal)
                if (result.outputPath) {
                    await appData.tauri.revealInExplorer(result.outputPath);
                }
            } else if (result && result.cancelled) {
                console.log('Export detected as cancelled');
                setWaveInfo(t('export.cancelled'));
                setIsExporting(false);
            } else {
                throw new Error(result?.message || t('export.error', { error: '' }));
            }
        } catch (error) {
            console.error('Export failed:', error);
            const errorStr = error.toString();
            if (errorStr.includes('EXPORT_CANCELLED') || error === 'EXPORT_CANCELLED') {
                setWaveInfo(t('export.cancelled'));
                setIsExporting(false);
                return;
            }
            setWaveInfo(t('export.error', { error: '' }));
            setIsExporting(false);
            await message(t('export.error', { error: error.message }), { title: t('dialog.error'), kind: 'error' });
        }
    };

    const handleCancelExport = async () => {
        console.log('Handling cancel export click...');
        // 瞬间关闭 UI，无需等待后端异步清理完成
        setIsExporting(false);
        setWaveInfo(t('status.cancelling'));
        await appData.tauri.cancelExport();
    };

    // 监听项目状态：如果所有片段都被删完了（在切片模式下），重置面板档位并停止播放
    useEffect(() => {
        if (currentFile && viewMode === 'fragmented' && stats.remaining <= 0.005) {
            // 1. 强行停止播放器，防止在无内容的轨道上继续“空转”
            if (appData.videoPlayer) {
                appData.videoPlayer.pause();
                appData.videoPlayer.seekTo(0);
            }

            // 2. 当内容删光时，重置右侧面板档位以便用户可以从低档位重新开始尝试
            if (committedIntensity !== 0) {
                console.log('[MainInterface] Content empty, resetting intensity.');
                setIntensity(0.25);
                setCommittedIntensity(0);
                setWaveInfo(t('status.content_empty'));
            }
        }
    }, [currentFile, viewMode, stats.remaining, committedIntensity, appData.videoPlayer]);

    // 监听后端进度
    useEffect(() => {
        let unlistenFn = null;
        let isCancelled = false;
        
        const setupListener = async () => {
            if (appData.tauri && appData.tauri.listen) {
                const unlisten = await appData.tauri.listen('video-progress', (event) => {
                    const { percent, message, eta } = event.payload;
                    if (percent !== undefined) setExportProgress(percent);
                    if (message !== undefined) setExportMessage(message);
                    if (eta !== undefined) setExportEta(eta);
                });
                
                if (isCancelled) {
                    unlisten();
                } else {
                    unlistenFn = unlisten;
                }
            }
        };

        setupListener();
        return () => {
            isCancelled = true;
            if (unlistenFn) unlistenFn();
        };
    }, [appData.tauri]);

    // 实现虚拟剪辑播放逻辑：自动跳过已经“确认物理剪除”的静音片段
    useEffect(() => {
        if (!appData.videoPlayer) return;

        const handleTimeUpdate = (currentTime) => {
            // 只有在预览模式下且有已确认片段时才跳过
            if (viewMode !== 'fragmented' || !mergedSilences || mergedSilences.length === 0) return;

            // 检查当前时间是否落入任何已确定的静音区间
            // 使用已经排好序并合并过的 mergedSilences 以确保逻辑严密
            const silSeg = mergedSilences.find(seg => currentTime >= (seg.start - 0.05) && currentTime < (seg.end - 0.01));
            
            if (silSeg) {
                // 关键逻辑：如果这个静音段是最后的结尾（即后面没有语音了）
                // 这种情况下不能向后跳（会跳到视频尽头导致黑屏或幻灯片继续），而应该定格在静音段的起点
                const isTrailingSilence = silSeg.end >= (videoDuration - 0.1);
                
                if (isTrailingSilence) {
                    appData.videoPlayer.pause();
                    appData.videoPlayer.seekTo(silSeg.start);
                    setWaveInfo('播放已送达最后一个有效片段');
                } else {
                    // 如果后面还有话，则正常闪现跳过
                    appData.videoPlayer.seekTo(silSeg.end);
                }
            }
        };

        appData.videoPlayer.on('timeupdate', handleTimeUpdate);
        return () => appData.videoPlayer.off('timeupdate', handleTimeUpdate);
    }, [viewMode, mergedSilences, videoDuration, appData.videoPlayer]);

    const handleDeleteTrack = (type) => {
        if (type === 'media') {
            // 真正的移除：清除所有分析和文件状态，彻底重置应用到初始状态
            setCurrentFile(null);
            setFileInfo({
                name: '--', size: '--', duration: '--:--', format: '--',
                hasVideo: true, hasAudio: true // 恢复默认值
            });
            setConfirmedSegments([]);
            setPendingSegments([]);
            setHistory([]); 
            setVideoDuration(0);
            setWaveInfo(t('status.waiting'));
            
            // 重置策略与 UI 状态
            setIntensity(0.25); // 恢复初始 Natural 档位
            setCommittedIntensity(0);
            setThreshold(-36.0);
            setIsAutoThreshold(true);
            setPadding(0.25);
            setExportEnabled(false);
            setExportProgress(0);
            
            // 重置全局状态单例
            appData.state.resetFileState();
            
            // 彻底销毁播放器状态
            if (appData.videoPlayer) {
                appData.videoPlayer.destroy();
            }
        } else if (type === 'video') {
            setFileInfo(prev => ({ ...prev, hasVideo: false }));
            appData.state.hasVideo = false;
        } else if (type === 'audio') {
            setFileInfo(prev => ({ ...prev, hasAudio: false }));
            appData.state.hasAudio = false;
            setConfirmedSegments([]);
            setWaveInfo(t('status.no_file'));
        }
    };

    return (
        <div className="main-container">
            {isExporting && (
                <ExportProgressModal 
                    progress={exportProgress} 
                    message={exportMessage}
                    onCancel={handleCancelExport}
                />
            )}
            {/* Using 100% instead of vh/vw to ensure it stays within root container */}
            <PanelGroup direction="vertical" style={{ height: '100%', width: '100%' }}>
                
                {/* 上方 Panel：限定最小高度 */}
                {/* 假设 RightPanel 最小需要 400px，在 1000px 屏幕下即为 40% */}
                <Panel defaultSize={60} minSize={40}>
                    <PanelGroup direction="horizontal" style={{ height: '100%', width: '100%' }}>
                        {/* 左：播放区（跟随垂直拉伸而此消彼长） */}
                        <Panel defaultSize={75} minSize={30}>
                            <LeftPanel 
                                appData={appData} 
                                currentFile={currentFile}
                                onFileSelect={handleFileSelect}
                                waveInfo={waveInfo}
                                setWaveInfo={setWaveInfo}
                                setFileInfo={setFileInfo}
                                setVideoDuration={setVideoDuration}
                                segments={confirmedSegments}
                                pendingSegments={pendingSegments}
                                viewMode={viewMode}
                                stats={stats}
                            />
                        </Panel>
                        
                        <PanelResizeHandle className="resize-handle-v" />
                        
                        {/* 右：侧边栏（固定宽度，高度占据上方 Panel 全部） */}
                        <Panel defaultSize={25} minSize={20} maxSize={40}>
                            <RightPanel 
                                appData={appData}
                                currentFile={currentFile}
                                fileInfo={fileInfo}
                                segments={confirmedSegments}
                                pendingSegments={pendingSegments}
                                intensity={intensity}
                                setIntensity={setIntensity}
                                threshold={threshold}
                                setThreshold={setThreshold}
                                isAutoThreshold={isAutoThreshold}
                                setIsAutoThreshold={setIsAutoThreshold}
                                stats={stats}
                                committedIntensity={committedIntensity}
                                exportEnabled={exportEnabled}
                                timeDisplay={timeDisplay}
                                viewMode={viewMode}
                                setViewMode={setViewMode}
                                onAnalyze={handleAnalyze}
                                onRemoveSilence={handleRemoveSilence}
                                onExport={handleExport}
                            />
                        </Panel>
                    </PanelGroup>
                </Panel>
                
                {/* 关键拉伸条：控制上方区域（Video+Sidebar）与下方波形区域的高度比例 */}
                <PanelResizeHandle className="resize-handle-h" />
                
                {/* 下方 Panel：波形区域（全宽） */}
                <Panel defaultSize={40} minSize={15}>
                    <div className="layout-bottom" style={{ height: '100%' }}>
                        <div className="bottom-waveform-area">
                            <WaveformSection 
                                appData={appData} 
                                currentFile={currentFile} 
                                videoDuration={videoDuration}
                                hasVideo={fileInfo.hasVideo}
                                hasAudio={fileInfo.hasAudio}
                                waveInfo={waveInfo}
                                setWaveInfo={setWaveInfo}
                                viewMode={viewMode}
                                timeline={timeline}
                                onDeleteMedia={() => handleDeleteTrack('media')}
                            />
                        </div>
                    </div>
                </Panel>
            </PanelGroup>
        </div>
    );
};

export default MainInterface;

