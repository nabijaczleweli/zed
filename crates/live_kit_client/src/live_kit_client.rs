mod remote_video_track_view;
#[cfg(any(test, feature = "test-support"))]
pub mod test;

use anyhow::{anyhow, Context as _, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait as _},
    StreamConfig,
};
use futures::{Stream, StreamExt as _};
use gpui::{AppContext, ScreenCaptureFrame, ScreenCaptureSource, ScreenCaptureStream, Task};
use parking_lot::Mutex;
use std::{borrow::Cow, sync::Arc};
use util::{ResultExt as _, TryFutureExt};
use webrtc::{
    audio_frame::AudioFrame,
    audio_source::{native::NativeAudioSource, AudioSourceOptions, RtcAudioSource},
    audio_stream::native::NativeAudioStream,
    video_frame::{VideoBuffer, VideoFrame, VideoRotation},
    video_source::{native::NativeVideoSource, RtcVideoSource, VideoResolution},
    video_stream::native::NativeVideoStream,
};

#[cfg(not(any(test, feature = "test-support", target_os = windows)))]
pub use livekit::*;
#[cfg(any(test, feature = "test-support", target_os = windows))]
pub use test::*;

pub use remote_video_track_view::{RemoteVideoTrackView, RemoteVideoTrackViewEvent};

pub struct AudioStream {
    _tasks: [Task<Option<()>>; 2],
}

struct Dispatcher(Arc<dyn gpui::PlatformDispatcher>);

impl livekit::dispatcher::Dispatcher for Dispatcher {
    fn dispatch(&self, runnable: livekit::dispatcher::Runnable) {
        self.0.dispatch(runnable, None);
    }

    fn dispatch_after(
        &self,
        duration: std::time::Duration,
        runnable: livekit::dispatcher::Runnable,
    ) {
        self.0.dispatch_after(duration, runnable);
    }
}

pub fn init(dispatcher: Arc<dyn gpui::PlatformDispatcher>) {
    livekit::dispatcher::set_dispatcher(Dispatcher(dispatcher));
}

pub async fn capture_local_video_track(
    capture_source: &dyn ScreenCaptureSource,
) -> Result<(track::LocalVideoTrack, Box<dyn ScreenCaptureStream>)> {
    let resolution = capture_source.resolution()?;
    let track_source = NativeVideoSource::new(VideoResolution {
        width: resolution.width.0 as u32,
        height: resolution.height.0 as u32,
    });

    let capture_stream = capture_source
        .stream({
            let track_source = track_source.clone();
            Box::new(move |frame| {
                if let Some(buffer) = video_frame_buffer_to_webrtc(frame) {
                    track_source.capture_frame(&VideoFrame {
                        rotation: VideoRotation::VideoRotation0,
                        timestamp_us: 0,
                        buffer,
                    });
                }
            })
        })
        .await??;

    Ok((
        track::LocalVideoTrack::create_video_track(
            "screen share",
            RtcVideoSource::Native(track_source),
        ),
        capture_stream,
    ))
}

pub fn capture_local_audio_track(
    cx: &mut AppContext,
) -> Result<(track::LocalAudioTrack, AudioStream)> {
    let (frame_tx, mut frame_rx) = futures::channel::mpsc::unbounded();

    let sample_rate;
    let channels;
    let stream;
    if cfg!(any(test, feature = "test-support")) {
        sample_rate = 1;
        channels = 1;
        stream = None;
    } else {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or_else(|| anyhow!("no audio input device available"))?;
        let config = device
            .default_input_config()
            .context("failed to get default input config")?;
        sample_rate = config.sample_rate().0;
        channels = config.channels() as u32;
        stream = Some(
            device
                .build_input_stream_raw(
                    &config.config(),
                    cpal::SampleFormat::I16,
                    move |data, _: &_| {
                        frame_tx
                            .unbounded_send(AudioFrame {
                                data: Cow::Owned(data.as_slice::<i16>().unwrap().to_vec()),
                                sample_rate,
                                num_channels: channels,
                                samples_per_channel: data.len() as u32 / channels,
                            })
                            .ok();
                    },
                    |err| log::error!("error capturing audio track: {:?}", err),
                    None,
                )
                .context("failed to build input stream")?,
        );
    }

    let source = NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain_control: false,
        },
        sample_rate,
        channels,
        Some(true),
    );

    let stream_task = cx.foreground_executor().spawn(async move {
        if let Some(stream) = &stream {
            stream.play().log_err();
        }
        futures::future::pending().await
    });

    let transmit_task = cx.background_executor().spawn({
        let source = source.clone();
        async move {
            while let Some(frame) = frame_rx.next().await {
                source.capture_frame(&frame).await.ok();
            }
            Some(())
        }
    });

    let track =
        track::LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(source));

    Ok((
        track,
        AudioStream {
            _tasks: [stream_task, transmit_task],
        },
    ))
}

pub fn play_remote_audio_track(
    track: &track::RemoteAudioTrack,
    cx: &mut AppContext,
) -> AudioStream {
    let buffer = Arc::new(Mutex::new(Vec::<i16>::new()));
    let (stream_config_tx, mut stream_config_rx) = futures::channel::mpsc::unbounded();
    let mut stream = NativeAudioStream::new(track.rtc_track());

    let receive_task = cx.background_executor().spawn({
        let buffer = buffer.clone();
        async move {
            let mut stream_config = None;
            while let Some(frame) = stream.next().await {
                let mut buffer = buffer.lock();
                let buffer_size = frame.samples_per_channel * frame.num_channels;
                debug_assert!(frame.data.len() == buffer_size as usize);

                let frame_config = StreamConfig {
                    channels: frame.num_channels as u16,
                    sample_rate: cpal::SampleRate(frame.sample_rate),
                    buffer_size: cpal::BufferSize::Fixed(buffer_size),
                };

                if stream_config.as_ref().map_or(true, |c| *c != frame_config) {
                    buffer.resize(buffer_size as usize, 0);
                    stream_config = Some(frame_config.clone());
                    stream_config_tx.unbounded_send(frame_config).ok();
                }

                if frame.data.len() == buffer.len() {
                    buffer.copy_from_slice(&frame.data);
                } else {
                    buffer.iter_mut().for_each(|x| *x = 0);
                }
            }
            Some(())
        }
    });

    let play_task = cx.foreground_executor().spawn(
        {
            let buffer = buffer.clone();
            async move {
                if cfg!(any(test, feature = "test-support")) {
                    return Err(anyhow!("can't play audio in tests"));
                }

                let device = cpal::default_host()
                    .default_output_device()
                    .ok_or_else(|| anyhow!("no audio output device available"))?;

                let mut _output_stream = None;
                while let Some(config) = stream_config_rx.next().await {
                    _output_stream = Some(device.build_output_stream(
                        &config,
                        {
                            let buffer = buffer.clone();
                            move |data, _info| {
                                let buffer = buffer.lock();
                                if data.len() == buffer.len() {
                                    data.copy_from_slice(&buffer);
                                } else {
                                    data.iter_mut().for_each(|x| *x = 0);
                                }
                            }
                        },
                        |error| log::error!("error playing audio track: {:?}", error),
                        None,
                    )?);
                }

                Ok(())
            }
        }
        .log_err(),
    );

    AudioStream {
        _tasks: [receive_task, play_task],
    }
}

pub fn play_remote_video_track(
    track: &track::RemoteVideoTrack,
) -> impl Stream<Item = ScreenCaptureFrame> {
    NativeVideoStream::new(track.rtc_track())
        .filter_map(|frame| async move { video_frame_buffer_from_webrtc(frame.buffer) })
}

#[cfg(target_os = "macos")]
fn video_frame_buffer_from_webrtc(buffer: Box<dyn VideoBuffer>) -> Option<ScreenCaptureFrame> {
    use core_foundation::base::TCFType as _;
    use media::core_video::CVImageBuffer;

    let buffer = buffer.as_native()?;
    let pixel_buffer = buffer.get_cv_pixel_buffer();
    if pixel_buffer.is_null() {
        return None;
    }

    unsafe {
        Some(ScreenCaptureFrame(CVImageBuffer::wrap_under_get_rule(
            pixel_buffer as _,
        )))
    }
}

#[cfg(not(target_os = "macos"))]
fn video_frame_buffer_from_webrtc(_buffer: Box<dyn VideoBuffer>) -> Option<ScreenCaptureFrame> {
    None
}

#[cfg(target_os = "macos")]
fn video_frame_buffer_to_webrtc(frame: ScreenCaptureFrame) -> Option<impl AsRef<dyn VideoBuffer>> {
    use core_foundation::base::TCFType as _;

    let pixel_buffer = frame.0.as_concrete_TypeRef();
    std::mem::forget(frame.0);
    unsafe {
        Some(webrtc::video_frame::native::NativeBuffer::from_cv_pixel_buffer(pixel_buffer as _))
    }
}

#[cfg(not(target_os = "macos"))]
fn video_frame_buffer_to_webrtc(_frame: ScreenCaptureFrame) -> Option<impl AsRef<dyn VideoBuffer>> {
    None as Option<Box<dyn VideoBuffer>>
}
