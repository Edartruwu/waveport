# Two seconds of silence, and a request for their final transcript.
# Recorded from the provider on 15 November 2026.
< {"type":"session.created","event_id":"event_Eka1AJlNvwul3OQ909Uoq","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_Eka1A3MlbFewnwVpv7vyI","expires_at":1794773848,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":null,"noise_reduction":null,"turn_detection":{"type":"server_vad","threshold":0.5,"prefix_padding_ms":300,"silence_duration_ms":200}}},"include":null}}
> "type":"session.update"
< {"type":"session.updated","event_id":"event_Eka1BnSEUo7vVFFmpCtpi","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_Eka1A3MlbFewnwVpv7vyI","expires_at":1794773848,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":{"model":"gpt-live-transcribe","language":null,"languages":null,"prompt":null},"noise_reduction":null,"turn_detection":null}},"include":null}}
> "type":"input_audio_buffer.commit"
< {"type":"input_audio_buffer.committed","event_id":"event_Eka1DICn5SW334I1eKMXh","previous_item_id":null,"item_id":"item_Eka1BpxWaSJwP4lAlATxf"}
< {"type":"conversation.item.added","event_id":"event_Eka1DSHPNGLEEwk27cLCC","previous_item_id":null,"item":{"id":"item_Eka1BpxWaSJwP4lAlATxf","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.done","event_id":"event_Eka1DvjWnu2tnbXtbIU6u","previous_item_id":null,"item":{"id":"item_Eka1BpxWaSJwP4lAlATxf","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.input_audio_transcription.completed","event_id":"event_Eka1DSialBlwEnC4Xf4yV","item_id":"item_Eka1BpxWaSJwP4lAlATxf","content_index":0,"transcript":"","usage":{"type":"duration","seconds":2}}
