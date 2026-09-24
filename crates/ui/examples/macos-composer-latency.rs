//! Keystroke-to-frame cost of the composer inside the real Shell, with native
//! CoreText/Metal (offscreen) and a long transcript loaded. Each keystroke is
//! timed from dispatch through draw and present, which is the main-thread work
//! the user waits on before vsync. Idle frames between keystrokes (caret,
//! height animation) are timed separately.
//!
//! Usage: macos-composer-latency OUTPUT_DIR
//! Env:   ZERON_LATENCY_REPEAT       transcript copies of the 52KB fixture (default 3)
//!        ZERON_LATENCY_PREFILL      bytes of multi-line draft pasted before typing (default 0)
//!        ZERON_LATENCY_CHARS        keystrokes to type (default 300)
//!        ZERON_LATENCY_INTERVAL_MS  gap between keystrokes (default 60)
//!        ZERON_LATENCY_STREAM       stream a live reply at ~30Hz while typing (default off)
//!        ZERON_LATENCY_SUBAGENTS    running subagent spawns in the last reply (default 0)
use gpui::{AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use zeron_ui::*;

#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("This profiler requires macOS")
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn fixture_reply() -> String {
    let raw = include_str!("../../../scripts/fixtures/resource-stream.jsonl");
    raw.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["event"]["type"] == "textDelta")
        .filter_map(|v| v["event"]["text"].as_str().map(str::to_owned))
        .collect()
}

fn running_spawn(ix: usize) -> zeron_doc::MessagePart {
    let task = [
        "audit the rail cache",
        "profile streaming frames",
        "draft release notes",
    ][ix % 3];
    zeron_doc::MessagePart::Tool {
        id: format!("spawn-{ix}"),
        call: zeron_proto::ToolCall::Unknown {
            name: format!("Agent: {task}"),
            input: Some(serde_json::json!({ "description": task })),
        },
        is_error: false,
        resolved: true,
        output: None,
        diff: None,
        output_ref: None,
        output_bytes: None,
        diff_ref: None,
        diff_stats: None,
        subagent_ref: Some(format!("subagent-doc-{ix}")),
        subagent_status: Some(zeron_doc::SubagentStatus::Running),
        subagent_tail: Some(format!("Reading crates/ui/src/transcript.rs ({})", ix + 1)),
    }
}

fn transcript(repeat: usize, subagents: usize) -> zeron_doc::TranscriptFrame {
    let reply = fixture_reply();
    let mut entries = Vec::new();
    for turn in 0..repeat {
        for (role, text) in [
            (
                zeron_doc::MessageRole::User,
                "Write a detailed tutorial on Rust ownership with 80 numbered sections."
                    .to_string(),
            ),
            (zeron_doc::MessageRole::Assistant, reply.clone()),
        ] {
            let id = format!("m{}", entries.len());
            entries.push(zeron_doc::SessionMessageEntry {
                id: id.clone(),
                role,
                parts: vec![zeron_doc::MessagePart::Text {
                    id: format!("{id}-p0"),
                    text,
                }],
                created_at: 1_757_000_000_000 + turn as i64 * 60_000,
                device_id: "local".into(),
                status: Some(zeron_doc::MessageStatus::Complete),
                continuation_of: None,
                duration_ms: None,
            });
        }
    }
    if let Some(last) = entries.last_mut() {
        last.parts.extend((0..subagents).map(running_spawn));
    }
    zeron_doc::TranscriptFrame::reset(&entries)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn summary(mut samples: Vec<f64>) -> serde_json::Value {
    samples.sort_by(f64::total_cmp);
    let mean = samples.iter().sum::<f64>() / samples.len().max(1) as f64;
    serde_json::json!({
        "n": samples.len(),
        "mean_ms": mean,
        "p50_ms": percentile(&samples, 0.50),
        "p95_ms": percentile(&samples, 0.95),
        "p99_ms": percentile(&samples, 0.99),
        "max_ms": samples.last().copied().unwrap_or(0.0),
    })
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(args.len() == 2, "Usage: macos-composer-latency OUTPUT_DIR");
    let output = std::path::PathBuf::from(&args[1]);
    std::fs::create_dir_all(&output)?;
    let repeat = env_usize("ZERON_LATENCY_REPEAT", 3);
    let prefill = env_usize("ZERON_LATENCY_PREFILL", 0);
    let chars = env_usize("ZERON_LATENCY_CHARS", 300);
    let interval = Duration::from_millis(env_usize("ZERON_LATENCY_INTERVAL_MS", 60) as u64);
    let stream = env_usize("ZERON_LATENCY_STREAM", 0) != 0;
    let subagents = env_usize("ZERON_LATENCY_SUBAGENTS", 0);

    let native = gpui_platform::current_platform(true);
    let platform = gpui::bench_platform(
        Some(Box::new(gpui_platform::current_headless_renderer)),
        native.text_system(),
    );
    let executor = platform.background_executor();
    let handles = Rc::new(RefCell::new(None));
    let captured = handles.clone();
    let data = output.clone();
    let app = gpui::Application::with_platform(platform)
        .with_assets(icons::Assets)
        .run_embedded(move |cx| {
            gpui_tokio::init(cx);
            gpui_base::init(cx);
            let settings = settings::UiSettings::default();
            settings::init(settings.clone(), data.clone(), cx);
            let fonts = typography::register_fonts(cx);
            typography::init(
                settings.ui_font_family.clone(),
                settings.ui_font_size,
                settings.terminal_font_family.clone(),
                settings.terminal_font_size,
                settings.code_font_family.clone(),
                settings.code_font_size,
                fonts,
                cx,
            );
            theme_library::init(data.clone(), cx);
            appearance::init(
                appearance::AppearanceMode::Dark,
                settings.theme_selection,
                settings.accent,
                settings.surface,
                cx,
            );
            history::init(
                settings.git_history_columns,
                settings.git_history_column_widths,
                settings.git_history_column_order,
                settings.git_history_author_display,
                cx,
            );
            composer::init(cx, settings.composer_send_behavior);
            terminal::panel::init(cx);
            let state = cx.new(|_| {
                let mut state = state::AppState::new();
                state.connection = zeron_proto::view::ConnectionStatus::Ready;
                state.workspace_scope = Some(zeron_proto::WorkspaceScope::Local);
                state.selected_chat = Some("profile".into());
                state.selected_space = Some("project".into());
                state.auto_selected = true;
                state.chats_synced = true;
                state.spaces_synced = true;
                state.spaces = vec![serde_json::from_value(serde_json::json!({
                    "id":"project", "deviceId":"local", "path":"/tmp/composer-latency",
                    "createdAt":"2026-09-05T00:00:00Z"
                }))
                .unwrap()];
                state.chats = vec![serde_json::from_value(serde_json::json!({
                    "id":"profile", "deviceId":"local", "spaceId":"project", "title":"Composer latency",
                    "archived":false, "createdAt":"2026-09-05T00:00:00Z",
                    "config":{"harness":"claude-code", "model":"claude-opus-5-5", "reasoning":null, "sandbox":"workspace-write"}
                }))
                .unwrap()];
                state
            });
            let boot = EngineBootConfig {
                data_dir: data,
                ipc_port: 0,
                edge_url: String::new(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: HarnessId::ClaudeCode,
            };
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            gpui::point(px(0.), px(0.)),
                            size(px(1320.), px(880.)),
                        ))),
                        ..Default::default()
                    },
                    |_, cx| cx.new(|cx| shell::Shell::new(state.clone(), boot, cx)),
                )
                .unwrap();
            *captured.borrow_mut() = Some((state, window));
        });
    let (state, window) = handles.borrow_mut().take().unwrap();
    let dispatcher = executor.dispatcher().as_bench().unwrap();

    let frame = |app: &gpui::ApplicationHandle| {
        objc::rc::autoreleasepool(|| {
            app.update(|cx| {
                window
                    .update(cx, |_, window, cx| window.simulate_next_frame(cx))
                    .unwrap()
            });
            dispatcher.run_until_idle();
            app.update(|cx| {
                window
                    .update(cx, |_, window, _| window.present_if_needed())
                    .unwrap()
            });
        })
    };
    let settle = |app: &gpui::ApplicationHandle, for_: Duration| {
        let start = Instant::now();
        while start.elapsed() < for_ {
            frame(app);
            std::thread::sleep(Duration::from_millis(8));
        }
    };

    app.update(|cx| {
        state.update(cx, |state, cx| {
            state
                .receive_transcript_frame(transcript(repeat, subagents), cx)
                .unwrap();
        })
    });
    settle(&app, Duration::from_secs(2));

    app.update(|cx| {
        cx.update_window(window.into(), |_, window, cx| click(window, cx, 550., 839.))
            .unwrap()
    });
    settle(&app, Duration::from_millis(500));

    if prefill > 0 {
        let line = "the quick brown fox jumps over the lazy dog, then keeps running\n";
        let draft: String = line.repeat(prefill / line.len() + 1)[..prefill].to_string();
        app.update(|cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(draft));
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_keystroke(gpui::Keystroke::parse("cmd-v").unwrap(), cx);
            })
            .unwrap();
        });
        settle(&app, Duration::from_secs(1));
    }

    let entries = 2 * repeat;
    let reply = fixture_reply();
    let mut streamed = 0usize;
    let mut last_append = Instant::now();
    let mut stream_tick = |app: &gpui::ApplicationHandle| {
        if !stream || last_append.elapsed() < Duration::from_millis(33) {
            return;
        }
        last_append = Instant::now();
        let start = streamed;
        streamed = (streamed + 24).min(reply.len());
        while !reply.is_char_boundary(streamed) {
            streamed += 1;
        }
        let frame = zeron_doc::TranscriptFrame::Delta {
            upsert: vec![],
            append: vec![zeron_doc::TextAppend {
                entry: "live".into(),
                part: "live-p0".into(),
                text: reply[start..streamed].to_string(),
                len: streamed,
            }],
            remove: vec![],
            count: entries + 1,
        };
        app.update(|cx| {
            state.update(cx, |state, cx| {
                state.receive_transcript_frame(frame, cx).unwrap()
            })
        });
    };
    if stream {
        let live = zeron_doc::SessionMessageEntry {
            id: "live".into(),
            role: zeron_doc::MessageRole::Assistant,
            parts: vec![zeron_doc::MessagePart::Text {
                id: "live-p0".into(),
                text: String::new(),
            }],
            created_at: 1_757_100_000_000,
            device_id: "local".into(),
            status: Some(zeron_doc::MessageStatus::Streaming),
            continuation_of: None,
            duration_ms: None,
        };
        let frame = zeron_doc::TranscriptFrame::Delta {
            upsert: vec![zeron_doc::TranscriptUpsert {
                after: Some(format!("m{}", entries - 1)),
                entry: live,
            }],
            append: vec![],
            remove: vec![],
            count: entries + 1,
        };
        app.update(|cx| {
            state.update(cx, |state, cx| {
                state.receive_transcript_frame(frame, cx).unwrap()
            })
        });
        settle(&app, Duration::from_millis(300));
    }

    let text = "make the composer feel as fast as a terminal, then check the numbers again. "
        .chars()
        .cycle()
        .take(chars);
    let mut keystroke_ms = Vec::new();
    let mut dispatch_ms = Vec::new();
    let mut idle_frame_ms = Vec::new();
    for ch in text {
        let key = match ch {
            ' ' => "space".to_string(),
            c => c.to_string(),
        };
        let keystroke = gpui::Keystroke::parse(&key).unwrap();
        let started = Instant::now();
        let dispatched = objc::rc::autoreleasepool(|| {
            let handled = app.update(|cx| {
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_keystroke(keystroke, cx)
                })
                .unwrap()
            });
            assert!(handled, "composer did not accept {key:?}");
            dispatcher.run_until_idle();
            started.elapsed()
        });
        frame(&app);
        keystroke_ms.push(started.elapsed().as_secs_f64() * 1e3);
        dispatch_ms.push(dispatched.as_secs_f64() * 1e3);
        while started.elapsed() + Duration::from_millis(8) < interval {
            std::thread::sleep(Duration::from_millis(8));
            stream_tick(&app);
            let idle = Instant::now();
            frame(&app);
            idle_frame_ms.push(idle.elapsed().as_secs_f64() * 1e3);
        }
    }
    settle(&app, Duration::from_millis(300));
    app.update(|cx| {
        window
            .update(cx, |_, window, _| {
                window
                    .render_to_image()
                    .unwrap()
                    .save(output.join("typed.png"))
                    .unwrap();
            })
            .unwrap()
    });

    if env_usize("ZERON_LATENCY_CLICK_SUBAGENT", 0) != 0 {
        app.update(|cx| {
            cx.update_window(window.into(), |_, window, cx| click(window, cx, 490., 747.))
                .unwrap()
        });
        settle(&app, Duration::from_millis(800));
        app.update(|cx| {
            window
                .update(cx, |_, window, _| {
                    window
                        .render_to_image()
                        .unwrap()
                        .save(output.join("subagent-opened.png"))
                        .unwrap();
                })
                .unwrap()
        });
    }

    // Opt-in: the reused (cached) scene must match a forced full re-render.
    let verify = (env_usize("ZERON_LATENCY_VERIFY", 0) != 0).then(|| {
        let cached = app.update(|cx| {
            window
                .update(cx, |_, window, _| window.render_to_image().unwrap())
                .unwrap()
        });
        app.update(|cx| window.update(cx, |_, window, _| window.refresh()).unwrap());
        dispatcher.run_until_idle();
        let fresh = app.update(|cx| {
            window
                .update(cx, |_, window, _| window.render_to_image().unwrap())
                .unwrap()
        });
        cached.save(output.join("verify-cached.png")).unwrap();
        fresh.save(output.join("verify-fresh.png")).unwrap();
        let mut differing = 0usize;
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0, 0);
        for (x, y, pixel) in cached.enumerate_pixels() {
            if pixel != fresh.get_pixel(x, y) {
                differing += 1;
                (min_x, min_y) = (min_x.min(x), min_y.min(y));
                (max_x, max_y) = (max_x.max(x), max_y.max(y));
            }
        }
        serde_json::json!({
            "differing_pixels": differing,
            "bounds": (differing > 0).then(|| [min_x, min_y, max_x, max_y]),
        })
    });

    let report = serde_json::json!({
        "verify_cached_vs_fresh": verify,
        "transcript_repeat": repeat,
        "prefill_bytes": prefill,
        "streaming": stream,
        "running_subagents": subagents,
        "interval_ms": interval.as_millis() as u64,
        "keystroke_to_frame": summary(keystroke_ms),
        "dispatch_only": summary(dispatch_ms),
        "idle_frames": summary(idle_frame_ms),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    std::fs::write(
        output.join("latency.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    drop(state);
    app.update(|cx| cx.quit());
    drop(app);
    Ok(())
}

#[cfg(target_os = "macos")]
fn click(window: &mut gpui::Window, cx: &mut gpui::App, x: f32, y: f32) {
    let position = gpui::point(px(x), px(y));
    window.dispatch_event(
        gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
            position,
            click_count: 1,
            ..Default::default()
        }),
        cx,
    );
    window.dispatch_event(
        gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
            position,
            click_count: 1,
            ..Default::default()
        }),
        cx,
    );
}
