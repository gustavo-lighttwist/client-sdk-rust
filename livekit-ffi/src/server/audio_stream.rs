// Copyright 2023 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::room::FfiTrack;
use super::FfiHandle;
use crate::{proto, server, FfiError, FfiHandleId, FfiResult};
use futures_util::StreamExt;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::prelude::*;
use log::warn;
use tokio::sync::oneshot;

pub struct FfiAudioStream {
    pub handle_id: FfiHandleId,
    pub stream_type: proto::AudioStreamType,

    #[allow(dead_code)]
    close_tx: oneshot::Sender<()>, // Close the stream on drop
}

impl FfiHandle for FfiAudioStream {}

impl FfiAudioStream {
    /// Setup a new AudioStream and forward the audio data to the client/the foreign
    /// language.
    ///
    /// When FfiAudioStream is dropped (When the corresponding handle_id is dropped), the task
    /// is being closed.
    ///
    /// It is possible that the client receives an AudioFrame after the task is closed. The client
    /// musts ignore it.
    pub fn setup(
        server: &'static server::FfiServer,
        new_stream: proto::NewAudioStreamRequest,
    ) -> FfiResult<proto::AudioStreamInfo> {
        let ffi_track = server.retrieve_handle::<FfiTrack>(new_stream.track_handle)?;
        let rtc_track = ffi_track.track.rtc_track();

        let MediaStreamTrack::Audio(rtc_track) = rtc_track else {
            return Err(FfiError::InvalidRequest("not an audio track".into()));
        };

        let (close_tx, close_rx) = oneshot::channel();
        let stream_type = new_stream.r#type();
        let audio_stream = match stream_type {
            #[cfg(not(target_arch = "wasm32"))]
            proto::AudioStreamType::AudioStreamNative => {
                let audio_stream = Self {
                    handle_id: server.next_id(),
                    stream_type,
                    close_tx,
                };

                let native_stream = NativeAudioStream::new(rtc_track);
                server.async_runtime.spawn(Self::native_audio_stream_task(
                    server,
                    audio_stream.handle_id,
                    native_stream,
                    close_rx,
                ));
                Ok::<FfiAudioStream, FfiError>(audio_stream)
            }
            _ => {
                return Err(FfiError::InvalidRequest(
                    "unsupported audio stream type".into(),
                ))
            }
        }?;

        // Store the new audio stream and return the info
        let info = proto::AudioStreamInfo::from(
            proto::FfiOwnedHandle {
                id: audio_stream.handle_id,
            },
            &audio_stream,
        );
        server.store_handle(audio_stream.handle_id, audio_stream);
        Ok(info)
    }

    async fn native_audio_stream_task(
        server: &'static server::FfiServer,
        stream_handle_id: FfiHandleId,
        mut native_stream: NativeAudioStream,
        mut close_rx: oneshot::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                _ = &mut close_rx => {
                    break;
                }
                frame = native_stream.next() => {
                    let Some(frame) = frame else {
                        break;
                    };

                    let handle_id = server.next_id();
                    let buffer_info = proto::AudioFrameBufferInfo::from(proto::FfiOwnedHandle{ id: handle_id }, &frame);
                    server.store_handle(handle_id, frame);

                    if let Err(err) = server.send_event(proto::ffi_event::Message::AudioStreamEvent(
                        proto::AudioStreamEvent {
                            source_handle: stream_handle_id,
                            message: Some(proto::audio_stream_event::Message::FrameReceived(
                                proto::AudioFrameReceived {
                                    frame: Some(buffer_info),
                                },
                            )),
                        },
                    )).await {
                        warn!("failed to send audio frame: {}", err);
                    }
                }
            }
        }
    }
}
